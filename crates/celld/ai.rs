//! fork: a node's Workers AI, the `ai` binding's `run()` (harness.js, `Ai`;
//! docs/services/workers-ai.md). Cloudflare's binding runs a model of its
//! catalog by its id; celld runs the models the node's operator names, each
//! on an endpoint the operator runs or chooses (a model installed on the
//! box's own machine, say), and answers in the catalog's shapes, so a
//! Worker written for Cloudflare calls it as it is. A model the file does
//! not name goes where its `"*"` entry says, or rejects, saying the node
//! has none.
//!
//! `CELLD_AI_FILE` names a JSON file that maps a catalog id to its endpoint:
//!
//! ```json
//! { "models": {
//!     "@cf/openai/whisper-large-v3-turbo": { "engine": "whisper.cpp", "origin": "http://host.lima.internal:8178" },
//!     "*": { "engine": "workers-ai", "origin": "http://127.0.0.1:8796" } } }
//! ```
//!
//! An engine is how celld speaks to the endpoint (`Engine`):
//!
//! - `whisper.cpp`: whisper.cpp's server (`/inference`), for the catalog's
//!   Whisper (`TRANSCRIBE`). Run it with `--convert` (ffmpeg), so it reads
//!   what browsers and phones record, and a VAD model, for `vad_filter`.
//! - `openai`: an OpenAI-compatible transcription server
//!   (`<base_url>/audio/transcriptions`, its `model`), for the catalog's
//!   Whisper: speaches, LocalAI, whisper.cpp's server on that path.
//! - `workers-ai`: an endpoint that answers as the binding does,
//!   `POST <origin>/run/<model>` with the model's JSON input, its answer
//!   passed on as it came (fragment's dev fake is one). It is the one
//!   engine `"*"` may name.
//!
//! Text and embeddings models are not translated here yet: a Worker that
//! can name an OpenAI-compatible endpoint itself (fragment's models config
//! does) needs no binding for them. A later engine plugs in as these do:
//! a catalog id, its translation in and out, an entry in the file.
//!
//! An endpoint is the node's own service, often on loopback or the
//! machine the node's VM runs on, which a Worker's fetch never reaches: the
//! host calls it, outside every isolate, and the Worker never sees its
//! address. whisper.cpp's `/load` swaps the model, so only the node may
//! reach it.
//!
//! A call is bounded (`INPUT_MAX_BYTES`, `AUDIO_MAX_BYTES`,
//! `ANSWER_MAX_BYTES`, `CALL_TIMEOUT`) and at most `concurrency` are in
//! flight to one endpoint; past that the call is answered 429, as Workers
//! AI answers one it has no capacity for, and its caller tries again.
//! Nothing is logged of an input or an answer.

use anyhow::{anyhow, bail};
use base64::Engine as _;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// The variable that names the node's AI file.
pub const VARIABLE: &str = "CELLD_AI_FILE";
/// The AI file, at most.
pub const FILE_MAX_BYTES: u64 = 64 * 1024;
/// The entry for every model the file does not name.
pub const OTHERWISE: &str = "*";
/// The catalog's Whisper (developers.cloudflare.com/workers-ai/models/
/// whisper-large-v3-turbo/): `{audio: <base64>, task?, language?,
/// vad_filter?, initial_prompt?, …}` in, `{text, word_count, segments, vtt,
/// transcription_info: {language, duration, …}}` out.
pub const TRANSCRIBE: &str = "@cf/openai/whisper-large-v3-turbo";
/// The audio of one call, at most (OpenAI's own transcription limit).
pub const AUDIO_MAX_BYTES: usize = 25 * 1024 * 1024;
/// A call's input as the isolate sends it: Whisper's audio as base64, and
/// room.
pub const INPUT_MAX_BYTES: usize = AUDIO_MAX_BYTES / 3 * 4 + 64 * 1024;
/// An endpoint's answer, at most: an hour's segments, or an image.
pub const ANSWER_MAX_BYTES: usize = 16 * 1024 * 1024;
/// One call, at most, the endpoint's queue included.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(600);
/// How many calls a transcription server has in flight, by default: it
/// runs one at a time.
pub const CONCURRENCY_DEFAULT: usize = 4;
/// And a `workers-ai` endpoint, which serves chat.
pub const CONCURRENCY_PASSED_DEFAULT: usize = 32;
/// And at most.
pub const CONCURRENCY_MAX: usize = 256;
/// A prompt, at most (Whisper reads 224 tokens of one).
const PROMPT_MAX_BYTES: usize = 4096;
/// A model's id or an endpoint's model, at most.
const MODEL_ID_MAX_BYTES: usize = 128;
/// The answer's headers a `workers-ai` endpoint's are passed on with,
/// beside its content type: what the binding's answer carries.
const PASSED_HEADERS: [&str; 2] = ["cf-aig-log-id", "cf-ai-neurons"];

/// Why a call is not answered: the binding rejects with `message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub code: &'static str,
    pub message: String,
}

fn failure(code: &'static str, message: impl Into<String>) -> Failure {
    Failure { code, message: message.into() }
}

/// An answer: a status, headers (the content type among them) and a body,
/// as Workers AI's would be.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

/// An answer's body: bytes celld made or read whole, or a `workers-ai`
/// endpoint's own, streamed to the isolate as it comes (a chat's events
/// reach its caller one by one, as on Cloudflare), at most
/// `ANSWER_MAX_BYTES`, its endpoint's slot held until it ends.
pub enum Body {
    Bytes(Vec<u8>),
    Stream(crate::engine_api::HttpChunkStream),
}

impl std::fmt::Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Body::Bytes(b) => write!(f, "Bytes({} bytes)", b.len()),
            Body::Stream(_) => write!(f, "Stream"),
        }
    }
}

impl Answer {
    fn json(status: u16, body: &Value) -> Answer {
        Answer { status, headers: vec![("content-type".into(), "application/json".into())], body: Body::Bytes(body.to_string().into_bytes()) }
    }

    /// The body as JSON, when it is one celld holds.
    pub fn value(&self) -> Option<Value> {
        match &self.body {
            Body::Bytes(b) => serde_json::from_slice(b).ok(),
            Body::Stream(_) => None,
        }
    }
}

/// A refusal in Workers AI's shape (`{errors: [{code, message}], success}`).
fn refused(status: u16, code: u32, message: impl Into<String>) -> Answer {
    Answer::json(status, &json!({ "success": false, "errors": [{ "code": code, "message": message.into() }] }))
}

/// How celld speaks to an endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Engine {
    WhisperCpp { origin: String },
    OpenAi { base_url: String, model: String },
    WorkersAi { origin: String },
}

impl Engine {
    fn transcribes(&self) -> bool {
        matches!(self, Engine::WhisperCpp { .. } | Engine::OpenAi { .. })
    }
}

/// One model's endpoint.
#[derive(Debug)]
pub struct Server {
    engine: Engine,
    slots: Arc<tokio::sync::Semaphore>,
}

/// The node's models, by catalog id, and where the rest go.
#[derive(Debug, Default)]
pub struct Models {
    servers: BTreeMap<String, Server>,
    otherwise: Option<Server>,
}

/// Whether `id` is a model's id as the catalog names one: segments of
/// letters, digits and `.`, `_`, `-`, `:`, `@` between single `/`s, none
/// empty, `.` or `..`, so it names a path under `/run/` and no other.
fn valid_model_id(id: &str) -> bool {
    (1..=MODEL_ID_MAX_BYTES).contains(&id.len())
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':' | b'/' | b'@'))
        && id.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
}

/// Whether `url` is an OpenAI-compatible API's base: an origin and a path,
/// no query, user or trailing `/` (celld adds `/audio/transcriptions`).
fn valid_base_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://")) else {
        return false;
    };
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let origin = &url[..url.len() - rest.len() + host.len()];
    crate::secrets::origin(origin)
        && !url.ends_with('/')
        && path.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'))
        && !path.split('/').any(|s| s == "." || s == "..")
}

impl Models {
    /// The models `text` (an AI file's contents, read from `file`) names, or
    /// why it is refused.
    pub fn parse(text: &[u8], file: &Path) -> anyhow::Result<Models> {
        let parsed: Value = serde_json::from_slice(text).map_err(|e| anyhow!("{VARIABLE} {} is not JSON: {e}", file.display()))?;
        let Some(top) = parsed.as_object() else {
            bail!("{VARIABLE} {} holds an object: {{\"models\": {{…}}}}", file.display());
        };
        if let Some(key) = top.keys().find(|k| *k != "models") {
            bail!("{VARIABLE} takes `models`, not `{key}`");
        }
        let Some(Value::Object(models)) = top.get("models") else {
            bail!("{VARIABLE} names its `models`: an object of each catalog id and its endpoint");
        };
        let mut out = Models::default();
        for (id, entry) in models {
            if id != OTHERWISE && !valid_model_id(id) {
                bail!("{VARIABLE} names {id:?}, which is no catalog id (@cf/…) nor \"*\"");
            }
            let Value::Object(entry) = entry else {
                bail!("{VARIABLE}'s {id} is an object naming its engine");
            };
            let text = |key: &str| entry.get(key).and_then(Value::as_str).unwrap_or_default();
            let (engine, keys): (Engine, &[&str]) = match text("engine") {
                "whisper.cpp" => (Engine::WhisperCpp { origin: text("origin").to_string() }, &["engine", "origin", "concurrency"]),
                "openai" => (Engine::OpenAi { base_url: text("base_url").to_string(), model: text("model").to_string() }, &["engine", "base_url", "model", "concurrency"]),
                "workers-ai" => (Engine::WorkersAi { origin: text("origin").to_string() }, &["engine", "origin", "concurrency"]),
                _ => bail!("{VARIABLE}'s {id} names its engine: \"whisper.cpp\", \"openai\" or \"workers-ai\""),
            };
            if let Some(key) = entry.keys().find(|k| !keys.contains(&k.as_str())) {
                bail!("{VARIABLE}'s {id} takes {}, not `{key}`", keys.join(", "));
            }
            match &engine {
                Engine::WhisperCpp { origin } | Engine::WorkersAi { origin } if !crate::secrets::origin(origin) => {
                    bail!("{VARIABLE}'s {id} names its endpoint's origin (http://127.0.0.1:8178, no path)")
                }
                Engine::OpenAi { base_url, .. } if !valid_base_url(base_url) => {
                    bail!("{VARIABLE}'s {id} names its server's base_url (http://127.0.0.1:8000/v1, no query or trailing /)")
                }
                Engine::OpenAi { model, .. } if !valid_model_id(model) => bail!("{VARIABLE}'s {id} names the server's `model`"),
                _ => {}
            }
            if engine.transcribes() && id != TRANSCRIBE {
                bail!("{VARIABLE}'s {id}: a transcription engine serves {TRANSCRIBE}; another model's endpoint is a \"workers-ai\" one");
            }
            let default = if engine.transcribes() { CONCURRENCY_DEFAULT } else { CONCURRENCY_PASSED_DEFAULT };
            let concurrency = match entry.get("concurrency") {
                None => default,
                Some(n) => match n.as_u64() {
                    Some(n) if (1..=CONCURRENCY_MAX as u64).contains(&n) => n as usize,
                    _ => bail!("{VARIABLE}'s {id} concurrency is 1 to {CONCURRENCY_MAX}"),
                },
            };
            let server = Server { engine, slots: Arc::new(tokio::sync::Semaphore::new(concurrency)) };
            if id == OTHERWISE {
                out.otherwise = Some(server);
            } else {
                out.servers.insert(id.clone(), server);
            }
        }
        Ok(out)
    }

    /// A slot on `model`'s endpoint for an input of `length` (its UTF-16
    /// units, as the isolate holds it), taken before the input is copied
    /// into the host: or the answer there is at once (too large, an id that
    /// is none, no endpoint, no capacity: Workers AI's 429).
    pub fn reserve(&self, model: &str, length: usize) -> Result<Reservation<'_>, Result<Answer, Failure>> {
        if length > INPUT_MAX_BYTES {
            return Err(Ok(refused(413, 5006, format!("the input is over {INPUT_MAX_BYTES} bytes"))));
        }
        if !valid_model_id(model) {
            return Err(Err(failure("invalid", "that is no model's id")));
        }
        let Some(server) = self.servers.get(model).or(self.otherwise.as_ref()) else {
            return Err(Err(failure("unbound", format!("this node has no Workers AI model {model}: its operator names none in {VARIABLE}"))));
        };
        let Ok(slot) = server.slots.clone().try_acquire_owned() else {
            return Err(Ok(refused(429, 3040, "Capacity temporarily exceeded, please try again.")));
        };
        Ok(Reservation { server, slot })
    }
}

/// A call's slot on its endpoint (`Models::reserve`), held until its answer
/// ends (a stream's last byte) or the call is dropped.
pub struct Reservation<'a> {
    server: &'a Server,
    slot: tokio::sync::OwnedSemaphorePermit,
}

impl Reservation<'_> {
    /// `model`'s answer to `inputs` (the isolate's JSON), or why there is
    /// none.
    pub async fn run(self, model: &str, inputs: &str) -> Result<Answer, Failure> {
        if inputs.len() > INPUT_MAX_BYTES {
            return Ok(refused(413, 5006, format!("the input is {} bytes; at most {INPUT_MAX_BYTES}", inputs.len())));
        }
        let engine = &self.server.engine;
        if let Engine::WorkersAi { origin } = engine {
            return passed(origin, model, inputs, self.slot).await;
        }
        let request = match whisper_request(inputs).and_then(|r| r.for_engine(engine)) {
            Ok(request) => request,
            Err(answer) => return Ok(answer),
        };
        match engine {
            Engine::WhisperCpp { origin } => transcribed(Dialect::WhisperCpp, origin, format!("{origin}/inference"), request).await,
            Engine::OpenAi { base_url, .. } => {
                let path = if request.translate { "translations" } else { "transcriptions" };
                transcribed(Dialect::OpenAi, base_url, format!("{base_url}/audio/{path}"), request).await
            }
            Engine::WorkersAi { .. } => unreachable!("answered above"),
        }
    }
}

/// A Whisper call, bounded, and the form an engine sends it as.
#[derive(Debug, Clone, PartialEq)]
pub struct WhisperRequest {
    pub audio: Vec<u8>,
    pub translate: bool,
    /// The language the call named (`None`: detect it).
    pub language: Option<String>,
    pub prompt: Option<String>,
    pub vad: bool,
    pub beam_size: Option<u32>,
    pub no_speech_threshold: Option<f64>,
    pub log_prob_threshold: Option<f64>,
    /// Its form's fields, as `for_engine` names them for its engine.
    pub fields: Vec<(&'static str, String)>,
}

/// The catalog's Whisper input, bounded, or the refusal Workers AI would
/// answer (400). A key of the catalog's that celld does not translate is
/// refused by name, never let go: the caller asked for it.
pub fn whisper_request(inputs: &str) -> Result<WhisperRequest, Answer> {
    let bad = |m: String| refused(400, 5006, m);
    let Ok(Value::Object(input)) = serde_json::from_str::<Value>(inputs) else {
        return Err(bad("the input is an object".into()));
    };
    const TRANSLATED: [&str; 8] = ["audio", "task", "language", "vad_filter", "initial_prompt", "beam_size", "no_speech_threshold", "log_prob_threshold"];
    const NOT_TRANSLATED: [&str; 4] = ["prefix", "condition_on_previous_text", "compression_ratio_threshold", "hallucination_silence_threshold"];
    for key in input.keys() {
        if NOT_TRANSLATED.contains(&key.as_str()) {
            return Err(bad(format!("celld's Whisper does not take `{key}`")));
        }
        if !TRANSLATED.contains(&key.as_str()) {
            return Err(bad(format!("Whisper's input has no `{key}`")));
        }
    }
    let audio = match input.get("audio") {
        Some(Value::String(b64)) => base64::engine::general_purpose::STANDARD.decode(b64).map_err(|_| bad("audio is base64".into()))?,
        Some(_) => return Err(bad("audio is a base64 string".into())),
        None => return Err(bad("the input carries its `audio`".into())),
    };
    if audio.is_empty() {
        return Err(bad("the audio is empty".into()));
    }
    if audio.len() > AUDIO_MAX_BYTES {
        return Err(refused(413, 5006, format!("the audio is {} bytes; at most {AUDIO_MAX_BYTES}", audio.len())));
    }
    let text = |key: &str| -> Result<Option<&str>, Answer> {
        match input.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.as_str())),
            Some(_) => Err(bad(format!("{key} is a string"))),
        }
    };
    let number = |key: &str| -> Result<Option<f64>, Answer> {
        match input.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v.as_f64().filter(|n| n.is_finite()).map(Some).ok_or_else(|| bad(format!("{key} is a number"))),
        }
    };
    let language = match text("language")?.map(str::trim).filter(|l| !l.is_empty()) {
        None => None,
        Some(l) if code_of(l).is_some() => Some(l.to_string()),
        Some(l) => return Err(bad(format!("{l:?} is no language Whisper knows"))),
    };
    let translate = match text("task")? {
        None | Some("transcribe") => false,
        Some("translate") => true,
        Some(_) => return Err(bad("task is transcribe or translate".into())),
    };
    let prompt = match text("initial_prompt")?.filter(|p| !p.trim().is_empty()) {
        Some(p) if p.len() > PROMPT_MAX_BYTES => return Err(bad(format!("initial_prompt is at most {PROMPT_MAX_BYTES} bytes"))),
        p => p.map(str::to_string),
    };
    let vad = match input.get("vad_filter") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(_) => return Err(bad("vad_filter is true or false".into())),
    };
    let beam_size = match number("beam_size")? {
        None => None,
        Some(n) if n.fract() == 0.0 && (1.0..=16.0).contains(&n) => Some(n as u32),
        Some(_) => return Err(bad("beam_size is 1 to 16".into())),
    };
    Ok(WhisperRequest {
        audio,
        translate,
        language,
        prompt,
        vad,
        beam_size,
        no_speech_threshold: number("no_speech_threshold")?,
        log_prob_threshold: number("log_prob_threshold")?,
        fields: Vec::new(),
    })
}

impl WhisperRequest {
    /// Its form for `engine`, or the refusal of what that engine cannot
    /// honour.
    fn for_engine(mut self, engine: &Engine) -> Result<WhisperRequest, Answer> {
        let mut f: Vec<(&'static str, String)> = Vec::new();
        match engine {
            Engine::WhisperCpp { .. } => {
                f.push(("language", self.language.clone().unwrap_or_else(|| "auto".into())));
                if self.translate {
                    f.push(("translate", "true".into()));
                }
                if let Some(p) = &self.prompt {
                    f.push(("prompt", p.clone()));
                }
                if self.vad {
                    f.push(("vad", "true".into()));
                }
                if let Some(n) = self.beam_size {
                    f.push(("beam_size", n.to_string()));
                }
                if let Some(n) = self.no_speech_threshold {
                    f.push(("no_speech_thold", n.to_string()));
                }
                if let Some(n) = self.log_prob_threshold {
                    f.push(("logprob_thold", n.to_string()));
                }
            }
            Engine::OpenAi { model, .. } => {
                for (named, key) in [(self.beam_size.is_some(), "beam_size"), (self.no_speech_threshold.is_some(), "no_speech_threshold"), (self.log_prob_threshold.is_some(), "log_prob_threshold")] {
                    if named {
                        return Err(refused(400, 5006, format!("this node's Whisper (an OpenAI-compatible server) takes no `{key}`")));
                    }
                }
                f.push(("model", model.clone()));
                // OpenAI's API detects a language it is not told
                if let Some(l) = &self.language {
                    f.push(("language", code_of(l).unwrap_or(l).to_string()));
                }
                if let Some(p) = &self.prompt {
                    f.push(("prompt", p.clone()));
                }
                // the server's own VAD, by the name faster-whisper's
                // servers take; one with none lets it go
                if self.vad {
                    f.push(("vad_filter", "true".into()));
                }
            }
            Engine::WorkersAi { .. } => unreachable!("a workers-ai endpoint takes the input as it is"),
        }
        f.push(("response_format", "verbose_json".into()));
        f.push(("temperature", "0".into()));
        self.fields = f;
        Ok(self)
    }
}

/// Whisper's languages, by code and by the name whisper.cpp and OpenAI
/// answer with (whisper.cpp's `g_lang`).
const LANGUAGES: &str = "en:english,zh:chinese,de:german,es:spanish,ru:russian,ko:korean,fr:french,ja:japanese,pt:portuguese,tr:turkish,pl:polish,ca:catalan,nl:dutch,ar:arabic,sv:swedish,it:italian,id:indonesian,hi:hindi,fi:finnish,vi:vietnamese,he:hebrew,uk:ukrainian,el:greek,ms:malay,cs:czech,ro:romanian,da:danish,hu:hungarian,ta:tamil,no:norwegian,th:thai,ur:urdu,hr:croatian,bg:bulgarian,lt:lithuanian,la:latin,mi:maori,ml:malayalam,cy:welsh,sk:slovak,te:telugu,fa:persian,lv:latvian,bn:bengali,sr:serbian,az:azerbaijani,sl:slovenian,kn:kannada,et:estonian,mk:macedonian,br:breton,eu:basque,is:icelandic,hy:armenian,ne:nepali,mn:mongolian,bs:bosnian,kk:kazakh,sq:albanian,sw:swahili,gl:galician,mr:marathi,pa:punjabi,si:sinhala,km:khmer,sn:shona,yo:yoruba,so:somali,af:afrikaans,oc:occitan,ka:georgian,be:belarusian,tg:tajik,sd:sindhi,gu:gujarati,am:amharic,yi:yiddish,lo:lao,uz:uzbek,fo:faroese,ht:haitian creole,ps:pashto,tk:turkmen,nn:nynorsk,mt:maltese,sa:sanskrit,lb:luxembourgish,my:myanmar,bo:tibetan,tl:tagalog,mg:malagasy,as:assamese,tt:tatar,haw:hawaiian,ln:lingala,ha:hausa,ba:bashkir,jw:javanese,su:sundanese,yue:cantonese";

/// The code of a language, named by its code or its name (any case).
fn code_of(language: &str) -> Option<&'static str> {
    let l = language.to_ascii_lowercase();
    LANGUAGES.split(',').filter_map(|pair| pair.split_once(':')).find(|(code, name)| *code == l || *name == l).map(|(code, _)| code)
}

/// Whose `verbose_json` a transcription is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    WhisperCpp,
    OpenAi,
}

/// A transcription server's `verbose_json` (whisper.cpp's, or OpenAI's) as
/// the catalog's answer: the text, its words counted, its segments and a
/// WebVTT of them, and what was heard (`transcription_info`: the language's
/// code and the audio's length, which a caller meters by). What the server
/// does not measure is left out, never made up: `duration_after_vad`, and
/// a segment's words' timings.
pub fn catalog_answer(dialect: Dialect, verbose: &Value, asked: Option<&str>) -> Option<Value> {
    let said = verbose["text"].as_str()?;
    let (text, duration) = match dialect {
        // whisper.cpp ends each segment's text with a newline, each begins
        // with its own space where the language has one, and it measures
        // the audio in single precision (2.2 s as 2.200000047683716): to
        // the millisecond, which its samples are
        Dialect::WhisperCpp => (said.replace('\n', "").trim().to_string(), verbose["duration"].as_f64().map(|d| (d * 1000.0).round() / 1000.0)),
        Dialect::OpenAi => (said.trim().to_string(), verbose["duration"].as_f64()),
    };
    let duration = duration.filter(|d| d.is_finite() && *d >= 0.0)?;
    let language = asked.or_else(|| verbose["language"].as_str()).and_then(code_of);
    let mut segments = Vec::new();
    let mut vtt = String::from("WEBVTT\n\n");
    for s in verbose["segments"].as_array().into_iter().flatten() {
        let (start, end) = (s["start"].as_f64().unwrap_or(0.0), s["end"].as_f64().unwrap_or(0.0));
        let said = s["text"].as_str().unwrap_or("").trim();
        let mut segment = json!({ "start": start, "end": end, "text": said });
        for key in ["temperature", "avg_logprob", "compression_ratio", "no_speech_prob"] {
            if let Some(v) = s.get(key).filter(|v| v.is_number()) {
                segment[key] = v.clone();
            }
        }
        segments.push(segment);
        vtt.push_str(&format!("{} --> {}\n{said}\n\n", stamp(start), stamp(end)));
    }
    let mut info = Map::new();
    if let Some(code) = language {
        info.insert("language".into(), json!(code));
    }
    if let Some(p) = verbose["detected_language_probability"].as_f64().filter(|p| asked.is_none() && p.is_finite()) {
        info.insert("language_probability".into(), json!(p));
    }
    info.insert("duration".into(), json!(duration));
    Some(json!({
        "text": text,
        "word_count": text.split_whitespace().count(),
        "segments": segments,
        "vtt": vtt,
        "transcription_info": info,
    }))
}

/// A WebVTT time, `HH:MM:SS.mmm`.
fn stamp(seconds: f64) -> String {
    let ms = (seconds.max(0.0) * 1000.0).round() as u64;
    format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// The client endpoints are called with: no proxy, no redirect, each call
/// bounded. Not a Worker's egress: an endpoint is the node's own.
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .tls_built_in_native_certs(false)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(CALL_TIMEOUT)
            .build()
            .expect("build the AI endpoints' HTTP client")
    })
}

/// `request` as a `multipart/form-data` body, its boundary 128 random
/// bits, checked once against the audio and the fields (`None`: they hold
/// it, which no caller makes happen).
fn form(request: &WhisperRequest) -> Option<(String, Vec<u8>)> {
    use rand::RngCore as _;
    let mut random = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut random);
    let boundary = format!("celld-ai-{}", random.iter().map(|b| format!("{b:02x}")).collect::<String>());
    let b = boundary.as_bytes();
    if request.audio.windows(b.len()).any(|w| w == b) || request.fields.iter().any(|(_, v)| v.contains(&boundary)) {
        return None;
    }
    let mut body = Vec::with_capacity(request.audio.len() + 4096);
    for (name, value) in &request.fields {
        body.extend_from_slice(format!("--{boundary}\r\ncontent-disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
    }
    body.extend_from_slice(format!("--{boundary}\r\ncontent-disposition: form-data; name=\"file\"; filename=\"audio\"\r\ncontent-type: application/octet-stream\r\n\r\n").as_bytes());
    body.extend_from_slice(&request.audio);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Some((boundary, body))
}

/// How long an endpoint's refusal is read, at most, and how much of it: a
/// refusal is answered at once, so its caller tries again in time.
const REFUSAL_READ: Duration = Duration::from_secs(2);
const REFUSAL_MAX_BYTES: usize = 4096;
/// How long a transcription's answer is read once its headers came: a
/// server sends it whole, after the inference, so a stall is a fault.
const ANSWER_READ: Duration = Duration::from_secs(30);

/// Why an endpoint gave no answer, for the Worker: the endpoint's address
/// stays in the node's log, never in what a Worker sees.
fn unanswered(model: &str, endpoint: &str, e: &reqwest::Error) -> Failure {
    tracing::warn!(model, endpoint, error = %e, "the AI endpoint did not answer");
    match e.is_timeout() {
        true => failure("down", format!("this node's endpoint for {model} did not answer within {} s", CALL_TIMEOUT.as_secs())),
        false => failure("down", format!("this node's endpoint for {model} is not answering")),
    }
}

/// An endpoint's body, read up to `max` bytes and no further.
async fn body_of(response: &mut reqwest::Response, max: usize) -> Result<Vec<u8>, reqwest::Error> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > max {
            bytes.extend_from_slice(&chunk[..max - bytes.len()]);
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// A transcription server (`endpoint`, at `url`) hears `request`; its
/// `verbose_json` is the catalog's answer. A refusal is answered as soon as
/// its status comes, its body read a little and briefly; a status its
/// caller may try again on (408, 429, 503) is kept, as is another 4xx, and
/// a server's failure is a 502.
async fn transcribed(dialect: Dialect, endpoint: &str, url: String, request: WhisperRequest) -> Result<Answer, Failure> {
    let Some((boundary, body)) = form(&request) else {
        return Ok(refused(400, 5006, "the audio holds the form's boundary"));
    };
    let sent = client()
        .post(url)
        .header("content-type", format!("multipart/form-data; boundary={boundary}"))
        .header("accept", "application/json")
        .body(body)
        .send()
        .await;
    let mut response = sent.map_err(|e| unanswered(TRANSCRIBE, endpoint, &e))?;
    let status = response.status().as_u16();
    let bytes = match status {
        200 => match crate::asyncrt::timeout(ANSWER_READ, body_of(&mut response, ANSWER_MAX_BYTES + 1)).await {
            Ok(Ok(b)) if b.len() > ANSWER_MAX_BYTES => return Err(failure("failed", format!("this node's Whisper answered over {ANSWER_MAX_BYTES} bytes"))),
            Ok(Ok(b)) => b,
            Ok(Err(e)) => return Err(unanswered(TRANSCRIBE, endpoint, &e)),
            Err(_) => return Err(failure("down", format!("this node's Whisper did not finish its answer within {} s", ANSWER_READ.as_secs()))),
        },
        _ => crate::asyncrt::timeout(REFUSAL_READ, body_of(&mut response, REFUSAL_MAX_BYTES)).await.ok().and_then(Result::ok).unwrap_or_default(),
    };
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    // whisper.cpp's `error` is a string; OpenAI's an object with a message
    let said: String = body["error"].as_str().or_else(|| body["error"]["message"].as_str()).unwrap_or("").chars().take(200).collect();
    if status != 200 {
        // its status alone: a refusal's text may echo what it was sent
        tracing::warn!(endpoint, status, "the AI endpoint refused a transcription");
    }
    Ok(match status {
        200 => match catalog_answer(dialect, &body, request.language.as_deref()) {
            Some(answer) => Answer::json(200, &answer),
            None => refused(502, 3000, "this node's Whisper answered with no text or length"),
        },
        // the audio is none ffmpeg reads: the call's, as Workers AI's 400
        500 if said.contains("FFmpeg conversion failed") => refused(400, 5006, "the audio could not be read"),
        // loading its model, or full: its caller tries again
        408 | 429 | 503 => refused(status, 3040, "this node's Whisper is busy; try again"),
        // its status, never its text, which may echo an address or the input
        400..=499 => refused(status, 5006, format!("this node's Whisper refused the call ({status})")),
        _ => refused(502, 3000, format!("this node's Whisper failed ({status})")),
    })
}

/// A `workers-ai` endpoint at `origin` runs `model` (a checked id: a path
/// under `/run/`) on `inputs`; its answer is passed on as it comes. A 200's
/// body is streamed, at most `ANSWER_MAX_BYTES`, its `slot` held until the
/// stream ends; a refusal's is read briefly and passed whole, so its caller
/// reads it at once.
async fn passed(origin: &str, model: &str, inputs: &str, slot: tokio::sync::OwnedSemaphorePermit) -> Result<Answer, Failure> {
    use futures_util::StreamExt as _;
    debug_assert!(valid_model_id(model), "Models::reserve checks the id");
    let sent = client().post(format!("{origin}/run/{model}")).header("content-type", "application/json").body(inputs.to_string()).send().await;
    let mut response = sent.map_err(|e| unanswered(model, origin, &e))?;
    let status = response.status().as_u16();
    let mut kept = Vec::new();
    for name in std::iter::once("content-type").chain(PASSED_HEADERS) {
        if let Some(v) = response.headers().get(name).and_then(|v| v.to_str().ok()) {
            kept.push((name.to_string(), v.to_string()));
        }
    }
    if status != 200 {
        let body = crate::asyncrt::timeout(REFUSAL_READ, body_of(&mut response, REFUSAL_MAX_BYTES)).await.ok().and_then(Result::ok).unwrap_or_default();
        return Ok(Answer { status, headers: kept, body: Body::Bytes(body) });
    }
    let capped = response.bytes_stream().scan((0usize, false, slot), |(read, ended, _slot), chunk| {
        if *ended {
            return futures_util::future::ready(None);
        }
        let out = match chunk {
            Ok(bytes) if *read + bytes.len() > ANSWER_MAX_BYTES => {
                *ended = true;
                Err(format!("the model's answer is over {ANSWER_MAX_BYTES} bytes"))
            }
            Ok(bytes) => {
                *read += bytes.len();
                Ok(bytes.to_vec())
            }
            Err(e) => {
                *ended = true;
                Err(format!("the model broke off its answer ({})", e.without_url()))
            }
        };
        futures_util::future::ready(Some(out))
    });
    Ok(Answer { status, headers: kept, body: Body::Stream(Box::pin(capped)) })
}

/// An answer, or why there is none, as the op hands it to the isolate: the
/// length of its metadata (4 bytes, big-endian), the metadata (`{status,
/// headers}`, with a `streamId` when its body is a stream `register` gave
/// one for, or `{code, message}`), then the body celld holds.
pub fn frame(answer: Result<Answer, Failure>, register: impl FnOnce(crate::engine_api::HttpChunkStream) -> Option<u64>) -> Vec<u8> {
    let (meta, body) = match answer {
        Ok(Answer { status, headers, body: Body::Bytes(body) }) => (json!({ "status": status, "headers": headers }), body),
        Ok(Answer { status, headers, body: Body::Stream(stream) }) => match register(stream) {
            Some(id) => (json!({ "status": status, "headers": headers, "streamId": id }), Vec::new()),
            None => (json!({ "code": "failed", "message": "the node is closing: the answer's stream was not opened" }), Vec::new()),
        },
        Err(f) => (json!({ "code": f.code, "message": f.message }), Vec::new()),
    };
    let meta = meta.to_string().into_bytes();
    let mut out = Vec::with_capacity(4 + meta.len() + body.len());
    out.extend_from_slice(&(meta.len() as u32).to_be_bytes());
    out.extend_from_slice(&meta);
    out.extend_from_slice(&body);
    out
}

/// The node's models, read once (`load`).
static MODELS: OnceLock<Option<Models>> = OnceLock::new();

#[allow(clippy::disallowed_methods)] // The operator's configuration file, not node storage.
fn from_env() -> anyhow::Result<Option<Models>> {
    use std::io::Read as _;
    let Some(path) = crate::env_vars::value(VARIABLE)? else {
        return Ok(None);
    };
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        bail!("{VARIABLE} is an absolute path, not {}", path.display());
    }
    let file = std::fs::File::open(&path).map_err(|e| anyhow!("{VARIABLE} {}: {e}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(FILE_MAX_BYTES + 1).read_to_end(&mut bytes).map_err(|e| anyhow!("{VARIABLE} {}: {e}", path.display()))?;
    if bytes.len() as u64 > FILE_MAX_BYTES {
        bail!("{VARIABLE} {} is over {FILE_MAX_BYTES} bytes", path.display());
    }
    Models::parse(&bytes, &path).map(Some)
}

/// Reads and checks the node's AI file, before the node starts: a file
/// that is wrong stops the process, saying why.
pub fn load() -> anyhow::Result<()> {
    if MODELS.get().is_none() {
        let models = from_env()?;
        let _ = MODELS.set(models);
    }
    Ok(())
}

/// A slot on `model`'s endpoint for an input of `length`, or the answer
/// there is at once (`Models::reserve`).
pub fn reserve(model: &str, length: usize) -> Result<Reservation<'static>, Result<Answer, Failure>> {
    match MODELS.get_or_init(|| from_env().expect("validated CELLD_AI_FILE")) {
        Some(models) => models.reserve(model, length),
        None => Err(Err(failure("unbound", format!("this node has no Workers AI model {model}: name its endpoint in {VARIABLE}")))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn cpp() -> Engine {
        Engine::WhisperCpp { origin: "http://127.0.0.1:8178".into() }
    }

    fn oai() -> Engine {
        Engine::OpenAi { base_url: "http://127.0.0.1:8000/v1".into(), model: "Systran/faster-whisper-small".into() }
    }

    fn fields(input: Value, engine: &Engine) -> Result<Vec<(&'static str, String)>, Answer> {
        whisper_request(&input.to_string()).and_then(|r| r.for_engine(engine)).map(|r| r.fields)
    }

    /// Valid: the fragment cell's input (audio, task, vad_filter, and a
    /// language and prompt when it has them) is each engine's form.
    #[test]
    fn the_catalogs_input_is_each_engines_form() {
        let cell = json!({ "audio": b64(b"OggS voice"), "task": "transcribe", "vad_filter": true });
        let r = whisper_request(&cell.to_string()).unwrap();
        assert_eq!((r.audio.as_slice(), r.language.as_deref()), (&b"OggS voice"[..], None), "no language: Whisper detects it");
        assert_eq!(fields(cell.clone(), &cpp()).unwrap(), vec![("language", "auto".into()), ("vad", "true".into()), ("response_format", "verbose_json".into()), ("temperature", "0".into())]);
        assert_eq!(
            fields(cell, &oai()).unwrap(),
            vec![("model", "Systran/faster-whisper-small".into()), ("vad_filter", "true".into()), ("response_format", "verbose_json".into()), ("temperature", "0".into())],
            "OpenAI's API detects a language it is not told: no `auto`"
        );
        let told = json!({ "audio": b64(b"x"), "language": "de", "initial_prompt": "Paul, fragment", "task": "translate", "beam_size": 5 });
        let f = fields(told.clone(), &cpp()).unwrap();
        for want in [("language", "de"), ("prompt", "Paul, fragment"), ("translate", "true"), ("beam_size", "5")] {
            assert!(f.contains(&(want.0, want.1.into())), "{want:?} in {f:?}");
        }
        assert!(!f.iter().any(|(k, _)| *k == "vad"), "no vad_filter: none");
        assert!(fields(told.clone(), &oai()).is_err_and(|a| a.status == 400 && a.value().unwrap().to_string().contains("beam_size")), "what the engine cannot honour is refused by name");
        let mut told = told;
        told.as_object_mut().unwrap().remove("beam_size");
        let f = fields(told, &oai()).unwrap();
        assert!(f.contains(&("language", "de".into())) && f.contains(&("prompt", "Paul, fragment".into())), "{f:?}");
        assert!(whisper_request(&json!({ "audio": b64(b"x"), "task": "translate" }).to_string()).unwrap().translate, "OpenAI's translations path");
    }

    /// Invalid: what Whisper's catalog has no input for, or celld does not
    /// translate, is refused by name (400), never let go; and audio that is
    /// none, or past its bound.
    #[test]
    fn an_input_out_of_shape_is_refused() {
        let status = |input: Value| whisper_request(&input.to_string()).err().map(|a| (a.status, a.value().unwrap()["errors"][0]["message"].as_str().unwrap_or("").to_string()));
        assert!(status(json!({})).is_some_and(|(s, m)| s == 400 && m.contains("audio")));
        assert!(status(json!({ "audio": "not base64!" })).is_some_and(|(s, _)| s == 400));
        assert!(status(json!({ "audio": [1, 2, 3] })).is_some_and(|(s, _)| s == 400), "the old Whisper's byte array is not this model's");
        assert!(status(json!({ "audio": "" })).is_some_and(|(s, _)| s == 400));
        assert!(status(json!({ "audio": b64(b"x"), "prefix": "so" })).is_some_and(|(s, m)| s == 400 && m.contains("prefix")));
        assert!(status(json!({ "audio": b64(b"x"), "temperature": 0 })).is_some_and(|(s, m)| s == 400 && m.contains("temperature")));
        assert!(status(json!({ "audio": b64(b"x"), "language": "klingon" })).is_some_and(|(s, _)| s == 400));
        assert!(status(json!({ "audio": b64(b"x"), "task": "summarize" })).is_some_and(|(s, _)| s == 400));
        assert!(status(json!({ "audio": b64(b"x"), "vad_filter": "yes" })).is_some_and(|(s, _)| s == 400));
        assert!(status(json!({ "audio": b64(b"x"), "beam_size": 0 })).is_some_and(|(s, _)| s == 400));
        let big = vec![0u8; AUDIO_MAX_BYTES + 1];
        assert!(status(json!({ "audio": b64(&big) })).is_some_and(|(s, _)| s == 413));
        assert!(whisper_request(&json!({ "audio": b64(&big[..AUDIO_MAX_BYTES]) }).to_string()).is_ok(), "the bound itself is taken");
    }

    /// A server's verbose_json (whisper.cpp's, OpenAI's) is the catalog's
    /// answer: its text, words, segments, a WebVTT, and the language's code
    /// and the audio's length; what it does not measure is left out. One
    /// without text or length is none.
    #[test]
    fn a_servers_answer_is_the_catalogs() {
        let verbose = json!({
            "task": "transcribe", "language": "english", "duration": 3.5, "text": " Hello there.\n General Kenobi.\n",
            "detected_language": "english", "detected_language_probability": 0.97,
            "segments": [
                { "id": 0, "text": " Hello there.", "start": 0.0, "end": 1.2, "tokens": [1], "words": [], "temperature": 0.0, "avg_logprob": -0.2, "no_speech_prob": 0.01 },
                { "id": 1, "text": " General Kenobi.", "start": 1.5, "end": 3.25, "tokens": [2], "temperature": 0.0, "avg_logprob": -0.3, "no_speech_prob": 0.02 }
            ]
        });
        let a = catalog_answer(Dialect::WhisperCpp, &verbose, None).unwrap();
        assert_eq!(a["text"], "Hello there. General Kenobi.");
        assert_eq!(a["word_count"], 4);
        assert_eq!(a["transcription_info"], json!({ "language": "en", "language_probability": 0.97, "duration": 3.5 }));
        assert_eq!(a["segments"][1], json!({ "start": 1.5, "end": 3.25, "text": "General Kenobi.", "temperature": 0.0, "avg_logprob": -0.3, "no_speech_prob": 0.02 }));
        assert_eq!(a["vtt"], "WEBVTT\n\n00:00:00.000 --> 00:00:01.200\nHello there.\n\n00:00:01.500 --> 00:00:03.250\nGeneral Kenobi.\n\n");
        assert!(a["transcription_info"].get("duration_after_vad").is_none(), "not measured, not made up");
        let asked = catalog_answer(Dialect::WhisperCpp, &verbose, Some("de")).unwrap();
        assert_eq!(asked["transcription_info"], json!({ "language": "de", "duration": 3.5 }), "the language it was told");
        // OpenAI's own shape: a compression ratio, a language that may be a code
        let openai = json!({ "task": "transcribe", "language": "English", "duration": 2.0, "text": "Hi.", "segments": [{ "id": 0, "seek": 0, "start": 0.0, "end": 2.0, "text": " Hi.", "tokens": [1], "temperature": 0.0, "avg_logprob": -0.1, "compression_ratio": 0.5, "no_speech_prob": 0.0 }] });
        let a = catalog_answer(Dialect::OpenAi, &openai, None).unwrap();
        assert_eq!((a["transcription_info"]["language"].clone(), a["segments"][0]["compression_ratio"].clone()), (json!("en"), json!(0.5)));
        let silent = catalog_answer(Dialect::WhisperCpp, &json!({ "text": "", "duration": 2.0, "segments": [] }), None).unwrap();
        assert_eq!((silent["text"].clone(), silent["word_count"].clone()), (json!(""), json!(0)), "silence is no words");
        assert_eq!(catalog_answer(Dialect::WhisperCpp, &json!({ "duration": 2.0 }), None), None);
        assert_eq!(catalog_answer(Dialect::WhisperCpp, &json!({ "text": "hi" }), None), None);
        assert_eq!(catalog_answer(Dialect::WhisperCpp, &json!({ "text": "hi", "duration": -1.0 }), None), None);
    }

    /// The form's boundary is in neither the audio nor a field.
    #[test]
    fn the_forms_boundary_is_its_own() {
        let r = whisper_request(&json!({ "audio": b64(b"x") }).to_string()).unwrap().for_engine(&cpp()).unwrap();
        let (first, body) = form(&r).unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.ends_with(&format!("--{first}--\r\n")) && text.contains("name=\"file\"; filename=\"audio\""));
        let (second, _) = form(&r).unwrap();
        assert_ne!(first, second, "random, not predictable");
        assert_eq!(first.len(), "celld-ai-".len() + 32);
    }

    /// The op's frame: its metadata's length, the metadata, the body.
    #[test]
    fn an_answer_is_framed_for_the_isolate() {
        let unused = |_: crate::engine_api::HttpChunkStream| -> Option<u64> { panic!("no stream to register") };
        let framed = frame(Ok(Answer { status: 200, headers: vec![("content-type".into(), "image/png".into())], body: Body::Bytes(vec![0x89, b'P']) }), unused);
        let n = u32::from_be_bytes(framed[..4].try_into().unwrap()) as usize;
        assert_eq!(serde_json::from_slice::<Value>(&framed[4..4 + n]).unwrap(), json!({ "status": 200, "headers": [["content-type", "image/png"]] }));
        assert_eq!(&framed[4 + n..], &[0x89, b'P']);
        let framed = frame(Err(failure("down", "gone")), unused);
        let n = u32::from_be_bytes(framed[..4].try_into().unwrap()) as usize;
        assert_eq!((serde_json::from_slice::<Value>(&framed[4..4 + n]).unwrap(), framed.len()), (json!({ "code": "down", "message": "gone" }), 4 + n));
    }

    /// The AI file: Whisper on a transcription engine, any model (or "*")
    /// on a workers-ai endpoint; anything else stops the node, saying what
    /// it takes.
    #[test]
    fn the_ai_file_names_each_models_endpoint() {
        let f = Path::new("/etc/celld/ai.json");
        let ok = Models::parse(
            br#"{"models":{"@cf/openai/whisper-large-v3-turbo":{"engine":"whisper.cpp","origin":"http://host.lima.internal:8178","concurrency":2},
                           "@cf/baai/bge-m3":{"engine":"workers-ai","origin":"http://127.0.0.1:8796"},
                           "*":{"engine":"workers-ai","origin":"http://127.0.0.1:8796"}}}"#,
            f,
        )
        .unwrap();
        assert_eq!(ok.servers[TRANSCRIBE].engine, Engine::WhisperCpp { origin: "http://host.lima.internal:8178".into() });
        assert_eq!(ok.servers[TRANSCRIBE].slots.available_permits(), 2);
        assert_eq!(ok.servers["@cf/baai/bge-m3"].slots.available_permits(), CONCURRENCY_PASSED_DEFAULT);
        assert_eq!(ok.otherwise.as_ref().unwrap().engine, Engine::WorkersAi { origin: "http://127.0.0.1:8796".into() });
        let openai = Models::parse(br#"{"models":{"@cf/openai/whisper-large-v3-turbo":{"engine":"openai","base_url":"http://127.0.0.1:8000/v1","model":"Systran/faster-whisper-small"}}}"#, f).unwrap();
        assert_eq!(openai.servers[TRANSCRIBE].engine, oai());
        for bad in [
            r#"[]"#,
            r#"{"models":{}, "extra": 1}"#,
            r#"{"models":{"@cf/meta/llama-3.1-8b-instruct":{"engine":"whisper.cpp","origin":"http://127.0.0.1:8178"}}}"#,
            r#"{"models":{"*":{"engine":"whisper.cpp","origin":"http://127.0.0.1:8178"}}}"#,
            r#"{"models":{"not a model":{"engine":"workers-ai","origin":"http://127.0.0.1:8796"}}}"#,
            r#"{"models":{"@cf/openai/whisper-large-v3-turbo":{"engine":"faster-whisper","origin":"http://127.0.0.1:8178"}}}"#,
            r#"{"models":{"@cf/openai/whisper-large-v3-turbo":{"engine":"whisper.cpp","origin":"http://127.0.0.1:8178/inference"}}}"#,
            r#"{"models":{"@cf/openai/whisper-large-v3-turbo":{"engine":"whisper.cpp","origin":"http://127.0.0.1:8178","concurrency":0}}}"#,
            r#"{"models":{"@cf/openai/whisper-large-v3-turbo":{"engine":"whisper.cpp","origin":"http://127.0.0.1:8178","threads":8}}}"#,
            r#"{"models":{"@cf/openai/whisper-large-v3-turbo":{"engine":"openai","base_url":"http://127.0.0.1:8000/v1/","model":"m"}}}"#,
            r#"{"models":{"@cf/openai/whisper-large-v3-turbo":{"engine":"openai","base_url":"http://127.0.0.1:8000/v1?x=1","model":"m"}}}"#,
            r#"{"models":{"@cf/openai/whisper-large-v3-turbo":{"engine":"openai","base_url":"http://127.0.0.1:8000/v1"}}}"#,
            r#"{"models":{"@cf/openai/whisper-large-v3-turbo":{"engine":"openai","origin":"http://127.0.0.1:8000","model":"m"}}}"#,
        ] {
            assert!(Models::parse(bad.as_bytes(), f).is_err(), "{bad}");
        }
    }

    /// A model's id is a path under /run/ and no other; the configured
    /// ones too.
    #[test]
    fn a_model_id_is_a_path_under_run() {
        for ok in ["@cf/openai/whisper-large-v3-turbo", "@cf/meta/llama-3.1-8b-instruct", "alibaba/hh1-t2v", "qwen3:8b"] {
            assert!(valid_model_id(ok), "{ok}");
        }
        for bad in ["", "..", "../admin", "@cf/../../admin", "@cf/./x", "a//b", "/a", "a/", "a b", "a?b", "a%2e%2e"] {
            assert!(!valid_model_id(bad), "{bad}");
        }
        let f = Path::new("/etc/celld/ai.json");
        assert!(Models::parse(br#"{"models":{"@cf/../x":{"engine":"workers-ai","origin":"http://127.0.0.1:1"}}}"#, f).is_err());
    }

    /// A call is refused before its input is copied: too large, an id that
    /// is none, no endpoint; and past its endpoint's concurrency, 429 until
    /// a slot is let go.
    #[tokio::test]
    async fn a_call_is_reserved_before_its_input_is_read() {
        let f = Path::new("/etc/celld/ai.json");
        let m = Models::parse(br#"{"models":{"@cf/openai/whisper-large-v3-turbo":{"engine":"whisper.cpp","origin":"http://127.0.0.1:1","concurrency":1}}}"#, f).unwrap();
        let status = |r: Result<Reservation<'_>, Result<Answer, Failure>>| match r {
            Ok(_) => "reserved".to_string(),
            Err(Ok(a)) => a.status.to_string(),
            Err(Err(f)) => f.code.to_string(),
        };
        assert_eq!(status(m.reserve(TRANSCRIBE, INPUT_MAX_BYTES + 1)), "413");
        assert_eq!(status(m.reserve("../admin", 10)), "invalid");
        assert_eq!(status(m.reserve("@cf/meta/llama-3.1-8b-instruct", 10)), "unbound", "no \"*\": another model has no endpoint");
        let held = m.reserve(TRANSCRIBE, 10);
        assert!(held.is_ok());
        assert_eq!(status(m.reserve(TRANSCRIBE, 10)), "429", "its one slot is held");
        drop(held);
        assert_eq!(status(m.reserve(TRANSCRIBE, 10)), "reserved", "and let go");
    }

    /// whisper.cpp's single-precision length is the millisecond its samples
    /// are, so a caller that meters by the millisecond charges no more;
    /// OpenAI's text keeps its own line breaks.
    #[test]
    fn whisper_cpps_length_is_to_the_millisecond() {
        // what whisper.cpp writes: the length as an f32, widened
        for (heard, ms) in [(f64::from(2.2_f32), 2.2), (f64::from(10.435_f32), 10.435), (f64::from(3.07_f32), 3.07), (f64::from(59.425_f32), 59.425)] {
            assert_ne!(heard, ms, "single precision is not the millisecond's");
            let a = catalog_answer(Dialect::WhisperCpp, &json!({ "text": "x", "duration": heard }), None).unwrap();
            assert_eq!(a["transcription_info"]["duration"], json!(ms), "{heard}");
        }
        let a = catalog_answer(Dialect::OpenAi, &json!({ "text": "First line.\nSecond line.", "duration": 2.0 }), None).unwrap();
        assert_eq!((a["text"].clone(), a["word_count"].clone()), (json!("First line.\nSecond line."), json!(4)));
    }

    #[test]
    fn languages_by_code_and_name() {
        assert_eq!(code_of("en"), Some("en"));
        assert_eq!(code_of("english"), Some("en"));
        assert_eq!(code_of("English"), Some("en"));
        assert_eq!(code_of("haitian creole"), Some("ht"));
        assert_eq!(code_of("yue"), Some("yue"));
        assert_eq!(code_of("xx"), None);
        assert_eq!(LANGUAGES.split(',').count(), 100);
    }
}
