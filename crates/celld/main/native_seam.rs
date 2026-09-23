// Fork seam (not upstream): a service binding whose target is
// `native:<name>` is answered by a native service from the fragment-native
// crate instead of a co-hosted script. Everything else about the call --
// the binding, the stub, the subrequest charge, the output gate -- is
// celld's ordinary service-binding path.

use celld::js::{HttpResponse, RequestBody, SvcCallReq};

pub(crate) async fn call(service: &'static dyn fragment_native::Service, call: SvcCallReq) {
    let SvcCallReq { caller, url, method, body, headers, reply, .. } = call;
    let body = match body {
        RequestBody::Bytes(bytes) => bytes.to_vec(),
        RequestBody::Stream(_) => {
            let _ = reply.send(Err(anyhow::anyhow!(
                "a native service takes a whole request body, not a stream"
            )));
            return;
        }
    };
    let answer = service
        .fetch(fragment_native::Request { caller, method, url, headers, body })
        .await;
    let _ = reply.send(Ok(HttpResponse {
        status: answer.status,
        body: answer.body,
        stream: None,
        headers: answer.headers,
        websocket: None,
        write_position: None,
        observed_position: None,
    }));
}
