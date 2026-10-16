//! Legacy uhttpd `/ubus` JSON-RPC, using ubus-zero's blobmsg argument/reply codec.
//! This is an HTTP client, not a native ubusd socket transport.

use alloc::{string::String, vec, vec::Vec};
use serde_json::{json, Value};
use ubus::{blobmsg, json::blobmsg_to_json, json::json_to_blobmsg, BlobMsgTable};

pub const MAX_RESPONSE: usize = 16 * 1024;
const MAX_REQUEST: usize = 2048;
const ANONYMOUS: &str = "00000000000000000000000000000000";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Transport,
    Timeout,
    Http(u16),
    TooLarge,
    InvalidReply,
    JsonRpc(i64),
    Ubus(u32),
}

impl Error {
    pub fn label(self) -> &'static str {
        match self {
            Self::Transport => "HTTP connection failed",
            Self::Timeout => "Router timed out",
            Self::Http(_) => "Check /ubus endpoint",
            Self::TooLarge => "Reply exceeds 16 KiB",
            Self::InvalidReply => "Invalid router reply",
            Self::JsonRpc(-32003) => "Router timed out",
            Self::JsonRpc(-32002) | Self::Ubus(6) => "Check password / ACL",
            Self::JsonRpc(-32000) | Self::Ubus(4) => "Object not available",
            Self::JsonRpc(_) => "JSON-RPC error",
            Self::Ubus(_) => "ubus call failed",
        }
    }

    fn session_expired(self) -> bool {
        matches!(self, Self::Ubus(6) | Self::JsonRpc(-32001 | -32002))
    }
}

#[allow(async_fn_in_trait)]
pub trait Http {
    /// Return the body length; never write beyond `response`. Implementations
    /// must bound the whole exchange with a deadline and reject oversized bodies.
    async fn post(&mut self, request: &[u8], response: &mut [u8]) -> Result<usize, Error>;
}

pub struct Client<H> {
    http: H,
    username: &'static str,
    password: &'static str,
    session: Option<String>,
    next_id: u32,
    response: Vec<u8>,
}

impl<H: Http> Client<H> {
    pub fn new(http: H, username: &'static str, password: &'static str) -> Self {
        Self {
            http,
            username,
            password,
            session: None,
            next_id: 1,
            response: vec![0; MAX_RESPONSE],
        }
    }

    pub async fn call(
        &mut self,
        object: &str,
        method: &str,
        args: &BlobMsgTable,
    ) -> Result<BlobMsgTable, Error> {
        if self.session.is_none() {
            self.login().await?;
        }
        let result = self.authenticated_call(object, method, args).await;
        if result.as_ref().is_err_and(|e| e.session_expired()) {
            // Expired sessions and ACL denials share status 6. Retry only once
            // so an actual denied method cannot create an authentication loop.
            self.session = None;
            self.login().await?;
            return self.authenticated_call(object, method, args).await;
        }
        result
    }

    async fn login(&mut self) -> Result<(), Error> {
        let args = BlobMsgTable::build(|b| {
            b.put_str(Some("username"), self.username);
            b.put_str(Some("password"), self.password);
            b.put_i32(Some("timeout"), 300);
        });
        let reply = self.request(ANONYMOUS, "session", "login", &args).await?;
        let token = blobmsg::lookup(reply.contents(), "ubus_rpc_session")
            .and_then(|a| a.as_str())
            .filter(|s| {
                s.len() == 32 && *s != ANONYMOUS && s.bytes().all(|b| b.is_ascii_hexdigit())
            })
            .ok_or(Error::InvalidReply)?;
        self.session = Some(String::from(token));
        Ok(())
    }

    async fn authenticated_call(
        &mut self,
        object: &str,
        method: &str,
        args: &BlobMsgTable,
    ) -> Result<BlobMsgTable, Error> {
        let session = self.session.clone().ok_or(Error::InvalidReply)?;
        self.request(&session, object, method, args).await
    }

    async fn request(
        &mut self,
        session: &str,
        object: &str,
        method: &str,
        args: &BlobMsgTable,
    ) -> Result<BlobMsgTable, Error> {
        let id = self.next_id;
        self.next_id = id.wrapping_add(1).max(1);
        let args = blobmsg_to_json(args.contents()).map_err(|_| Error::InvalidReply)?;
        let request = serde_json::to_vec(&json!({
            "jsonrpc": "2.0", "id": id, "method": "call",
            "params": [session, object, method, args]
        }))
        .map_err(|_| Error::InvalidReply)?;
        if request.len() > MAX_REQUEST {
            return Err(Error::TooLarge);
        }
        let n = self.http.post(&request, &mut self.response).await?;
        let body = self.response.get(..n).ok_or(Error::TooLarge)?;
        decode_reply(body, id)
    }
}

fn decode_reply(body: &[u8], id: u32) -> Result<BlobMsgTable, Error> {
    let reply: Value = serde_json::from_slice(body).map_err(|_| Error::InvalidReply)?;
    if reply["jsonrpc"] != "2.0" || reply["id"].as_u64() != Some(id.into()) {
        return Err(Error::InvalidReply);
    }
    if let Some(error) = reply.get("error") {
        if reply.get("result").is_some() {
            return Err(Error::InvalidReply);
        }
        return Err(Error::JsonRpc(
            error["code"].as_i64().ok_or(Error::InvalidReply)?,
        ));
    }
    let result = reply["result"].as_array().ok_or(Error::InvalidReply)?;
    if result.is_empty() || result.len() > 2 {
        return Err(Error::InvalidReply);
    }
    let status = result[0].as_u64().ok_or(Error::InvalidReply)?;
    let status = u32::try_from(status).map_err(|_| Error::InvalidReply)?;
    if status != 0 {
        return Err(Error::Ubus(status));
    }
    match result.get(1) {
        None => Ok(BlobMsgTable::empty()),
        Some(value) => json_to_blobmsg(value).map_err(|_| Error::InvalidReply),
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use alloc::collections::VecDeque;
    use core::{
        future::Future,
        task::{Context, Poll, Waker},
    };

    fn run<T>(future: impl Future<Output = T>) -> T {
        let mut future = core::pin::pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("mock HTTP must complete immediately"),
        }
    }

    struct Mock {
        replies: VecDeque<Value>,
        requests: Vec<Value>,
    }

    impl Http for Mock {
        async fn post(&mut self, request: &[u8], response: &mut [u8]) -> Result<usize, Error> {
            let request: Value = serde_json::from_slice(request).unwrap();
            let mut reply = self.replies.pop_front().expect("unexpected request");
            reply["id"] = request["id"].clone();
            self.requests.push(request);
            let bytes = serde_json::to_vec(&reply).unwrap();
            response[..bytes.len()].copy_from_slice(&bytes);
            Ok(bytes.len())
        }
    }

    fn login() -> Value {
        json!({"jsonrpc":"2.0", "result":[0, {"ubus_rpc_session":"1234567890abcdef1234567890abcdef"}]})
    }

    #[test]
    fn logs_in_escapes_credentials_and_reuses_session() {
        let mock = Mock {
            replies: [
                login(),
                json!({"jsonrpc":"2.0","result":[0,{"uptime":42}]}),
                json!({"jsonrpc":"2.0","result":[0]}),
            ]
            .into(),
            requests: vec![],
        };
        let mut client = Client::new(mock, "dashboard", "quote\"slash\\line\n");
        let reply = run(client.call("system", "info", &BlobMsgTable::empty())).unwrap();
        assert_eq!(blobmsg_to_json(reply.contents()).unwrap()["uptime"], 42);
        run(client.call("system", "board", &BlobMsgTable::empty())).unwrap();
        assert_eq!(client.http.requests.len(), 3);
        assert_eq!(
            client.http.requests[0]["params"][3]["password"],
            "quote\"slash\\line\n"
        );
        assert_eq!(
            client.http.requests[1]["params"][0],
            "1234567890abcdef1234567890abcdef"
        );
        assert_eq!(client.http.requests[2]["id"], 3);
    }

    #[test]
    fn expires_session_and_retries_denied_call_once() {
        for denied in [
            json!({"jsonrpc":"2.0","result":[6]}),
            json!({"jsonrpc":"2.0","error":{"code":-32001}}),
            json!({"jsonrpc":"2.0","error":{"code":-32002}}),
        ] {
            let mock = Mock {
                replies: [
                    login(),
                    denied,
                    login(),
                    json!({"jsonrpc":"2.0","result":[6]}),
                ]
                .into(),
                requests: vec![],
            };
            let mut client = Client::new(mock, "dashboard", "secret");
            assert_eq!(
                run(client.call("system", "info", &BlobMsgTable::empty())).err(),
                Some(Error::Ubus(6))
            );
            assert_eq!(client.http.requests.len(), 4);
        }
    }

    #[test]
    fn bad_password_stops_at_login() {
        let mock = Mock {
            replies: [json!({"jsonrpc":"2.0","result":[6]})].into(),
            requests: vec![],
        };
        let mut client = Client::new(mock, "dashboard", "bad");
        assert_eq!(
            run(client.call("system", "info", &BlobMsgTable::empty())).err(),
            Some(Error::Ubus(6))
        );
        assert_eq!(client.http.requests.len(), 1);
    }

    #[test]
    fn uhttpd_access_denied_renews_session_and_retries_with_new_token() {
        let new_token = "abcdef1234567890abcdef1234567890";
        let mock = Mock {
            replies: [
                login(),
                json!({"jsonrpc":"2.0","error":{"code":-32002}}),
                json!({"jsonrpc":"2.0","result":[0,{"ubus_rpc_session":new_token}]}),
                json!({"jsonrpc":"2.0","result":[0,{"uptime":100}]}),
            ]
            .into(),
            requests: vec![],
        };
        let mut client = Client::new(mock, "dashboard", "secret");
        let reply = run(client.call("system", "info", &BlobMsgTable::empty())).unwrap();
        assert_eq!(blobmsg_to_json(reply.contents()).unwrap()["uptime"], 100);
        assert_eq!(client.http.requests[3]["params"][0], new_token);
        assert_eq!(client.http.requests[3]["params"][1], "system");
        assert_eq!(client.http.requests[3]["params"][2], "info");
    }

    #[test]
    fn invalid_login_tokens_never_reach_authenticated_calls() {
        for token in ["", ANONYMOUS, "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"] {
            let mock = Mock {
                replies: [json!({"jsonrpc":"2.0","result":[0,{"ubus_rpc_session":token}]})].into(),
                requests: vec![],
            };
            let mut client = Client::new(mock, "dashboard", "secret");
            assert_eq!(
                run(client.call("system", "info", &BlobMsgTable::empty())).err(),
                Some(Error::InvalidReply)
            );
            assert_eq!(client.http.requests.len(), 1);
        }
    }

    #[test]
    fn validates_envelope_status_and_object_payload() {
        for reply in [
            json!({"jsonrpc":"2.0","id":2,"result":[0,{}]}),
            json!({"jsonrpc":"1.0","id":1,"result":[0,{}]}),
            json!({"jsonrpc":"2.0","id":1,"result":[]}),
            json!({"jsonrpc":"2.0","id":1,"result":[-1]}),
            json!({"jsonrpc":"2.0","id":1,"result":[0,[]]}),
            json!({"jsonrpc":"2.0","id":1,"result":[0,{},{}]}),
            json!({"jsonrpc":"2.0","id":1,"result":[0],"error":{"code":-1}}),
        ] {
            assert_eq!(
                decode_reply(&serde_json::to_vec(&reply).unwrap(), 1).err(),
                Some(Error::InvalidReply)
            );
        }
    }
}
