# Workers AI

A Worker runs a model of [Workers AI](https://developers.cloudflare.com/workers-ai/)
through its `ai` binding: `await env.AI.run(model, inputs, options)`. On celld
the binding is the same `Ai`, and a model the node's operator names is answered
by a server the operator runs beside the node, in the catalog's shapes. Every
other model rejects, "this node has no Workers AI model …". The host makes the
call, outside every isolate: the Worker never sees the server's address, and a
Worker's own `fetch` never reaches it (it is usually on loopback).

One model so far: Whisper, `@cf/openai/whisper-large-v3-turbo`, on
[whisper.cpp](https://github.com/ggml-org/whisper.cpp)'s server.

## Example

[`crates/celld/tests/workers_ai.rs`](../../crates/celld/tests/workers_ai.rs)
calls Whisper as fragment's cell does (an AI Gateway's options and
`returnRawResponse`), raw and as an object, against a stand-in of whisper.cpp's
server, and calls out of shape.

## Configuration

`CELLD_AI_FILE` names the node's AI file, an absolute path to a JSON file:

```json
{
  "models": {
    "@cf/openai/whisper-large-v3-turbo": {
      "engine": "whisper.cpp",
      "origin": "http://127.0.0.1:8178",
      "concurrency": 4
    }
  }
}
```

`origin` is the server's, with no path. `concurrency` (1 to 64, default 4) is
how many calls the node has in flight to it; whisper.cpp runs one at a time and
queues the rest, and a call past that is answered 429 with Workers AI's own
capacity error (3040), so a caller tries again as it does on Cloudflare. A file
that names another model, engine or key stops the node at start, saying what it
takes. Unset, the node has no models.

Run the server with the model of your choice (the catalog id is what the Worker
asks for; the weights are the operator's), `--convert` so it reads what
browsers and phones record (it needs `ffmpeg`), a VAD model for `vad_filter`,
and `-l auto`. Its `/load` replaces the model, so only the node may reach it.

```sh
whisper-server --host 127.0.0.1 --port 8178 -m ggml-small.bin -t 8 \
  -l auto --convert --vad-model ggml-silero-v6.2.0.bin -nlp
```

## Whisper

The input is the catalog's: `audio` (base64), `task` (`transcribe` or
`translate`), `language`, `vad_filter`, `initial_prompt`, `beam_size`,
`no_speech_threshold` and `log_prob_threshold`, sent to `/inference` as
whisper.cpp's form, at temperature 0. `prefix`,
`condition_on_previous_text`, `compression_ratio_threshold` and
`hallucination_silence_threshold` are refused by name: whisper.cpp's server
takes no such field. The audio is at most 25 MiB, and a call at most 600 s.

The answer is the catalog's: `text`, `word_count`, `segments` (`start`, `end`,
`text`, `temperature`, `avg_logprob`, `no_speech_prob`), `vtt`, and
`transcription_info` (`language` as its code, `duration` in seconds of audio,
and `language_probability` when the server measures it). What whisper.cpp does
not measure is left out, never made up: `duration_after_vad`, a segment's
`compression_ratio` and its words' timings.

With `returnRawResponse` the binding answers the Response Workers AI's REST API
would: 200 and the answer, or a refusal in its shape (`{success: false, errors:
[{code, message}]}`) with its status (400 for an input out of shape or audio
ffmpeg cannot read, 413 past the bound, 429 past `concurrency`, 503 while the
server loads its model, 502 for anything else it answered). Without it, the
answer's object, or a rejection (`InferenceUpstreamError`). A server that does
not answer rejects either way, naming it.

The options that say where a call is logged (`gateway`, `extraHeaders`,
`prefix`, `tags`) are taken and have no effect: a node has no AI Gateway.
`queueRequest` and `websocket` are refused by name.

Nothing of the audio or the words is logged.
