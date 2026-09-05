//! Application-neutral router. Only IDE discovery uses conservative all-provider
//! agreement; other methods retain the official router's first willing handler.
use super::protocol;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use uuid::Uuid;

pub type Peer = u64;
pub type Outgoing = Vec<(Peer, Value)>;
struct Client {
    id: String,
    kind: String,
}
struct Pending {
    source: Peer,
    request: Value,
    deadline: Instant,
    discoveries: HashMap<String, Peer>,
    willing: Vec<Peer>,
    target: Option<Peer>,
}
#[derive(Default)]
pub struct Router {
    clients: HashMap<Peer, Client>,
    pending: HashMap<String, Pending>,
}
impl Router {
    pub fn message(&mut self, peer: Peer, mut message: Value) -> Outgoing {
        let mut output = vec![];
        match message["type"].as_str() {
            Some("request") => {
                if !message["requestId"].is_string() || !message["method"].is_string() {
                    return output;
                }
                if message["method"] == "initialize" {
                    let Some(kind) = message["params"]["clientType"].as_str() else {
                        return vec![(peer, protocol::error(&message, "invalid-initialize"))];
                    };
                    if !self.clients.contains_key(&peer) {
                        self.clients.insert(
                            peer,
                            Client {
                                id: Uuid::new_v4().to_string(),
                                kind: kind.to_owned(),
                            },
                        );
                        output.extend(self.status(peer, "connected"));
                    }
                    let Some(client) = self.clients.get(&peer) else {
                        return output;
                    };
                    output.push((
                        peer,
                        protocol::success(&message, &client.id, json!({"clientId":client.id})),
                    ));
                    return output;
                }
                if self.pending.len() >= 128 {
                    return vec![(peer, protocol::error(&message, "router-busy"))];
                }
                if let Some(client) = self.clients.get(&peer) {
                    message["sourceClientId"] = json!(client.id);
                }
                let ide = message["method"] == "ide-context";
                let timeout = if ide {
                    protocol::REQUEST_BUDGET
                } else {
                    Duration::from_millis(
                        message["timeoutMs"]
                            .as_u64()
                            .unwrap_or(10_000)
                            .clamp(1, 60_000),
                    )
                };
                let mut pending = Pending {
                    source: peer,
                    request: message,
                    deadline: Instant::now() + timeout,
                    discoveries: HashMap::new(),
                    willing: vec![],
                    target: None,
                };
                for (&candidate, client) in &self.clients {
                    if candidate == peer {
                        continue;
                    }
                    if let Some(target) = pending.request["targetClientId"].as_str() {
                        if client.id != target {
                            continue;
                        }
                    }
                    let id = Uuid::new_v4().to_string();
                    output.push((candidate, json!({"type":"client-discovery-request", "requestId":id, "request":pending.request})));
                    pending.discoveries.insert(id, candidate);
                }
                if pending.discoveries.is_empty() {
                    output.push((peer, protocol::error(&pending.request, "no-client-found")));
                } else {
                    self.pending.insert(Uuid::new_v4().to_string(), pending);
                }
            }
            Some("client-discovery-response") => {
                let Some(id) = message["requestId"].as_str() else {
                    return output;
                };
                let key = self
                    .pending
                    .iter()
                    .find(|(_, p)| p.discoveries.get(id) == Some(&peer))
                    .map(|(key, _)| key.clone());
                let Some(key) = key else {
                    return output;
                };
                let Some(p) = self.pending.get_mut(&key) else {
                    return output;
                };
                p.discoveries.remove(id);
                if message["response"]["canHandle"] == true {
                    p.willing.push(peer);
                }
                let ide = p.request["method"] == "ide-context";
                if p.discoveries.is_empty() || (!ide && !p.willing.is_empty()) {
                    if p.willing.is_empty() || (ide && p.willing.len() > 1) {
                        let reason = if p.willing.is_empty() {
                            "no-client-found"
                        } else {
                            "ambiguous-ide-provider"
                        };
                        output.push((p.source, protocol::error(&p.request, reason)));
                        self.pending.remove(&key);
                    } else {
                        let target = p.willing[0];
                        p.target = Some(target);
                        p.discoveries.clear();
                        let mut request = p.request.clone();
                        request["requestId"] = json!(key);
                        output.push((target, request));
                    }
                }
            }
            Some("response") => {
                let Some(id) = message["requestId"].as_str().map(str::to_owned) else {
                    return output;
                };
                if self
                    .pending
                    .get(&id)
                    .is_some_and(|p| p.target == Some(peer))
                {
                    if let Some(p) = self.pending.remove(&id) {
                        message["requestId"] = p.request["requestId"].clone();
                        if message["resultType"] == "success" {
                            if let Some(client) = self.clients.get(&peer) {
                                message["handledByClientId"] = json!(client.id);
                            }
                        }
                        output.push((p.source, message));
                    }
                }
            }
            Some("broadcast") => {
                if let Some(client) = self.clients.get(&peer) {
                    message["sourceClientId"] = json!(client.id);
                }
                for (&other, client) in &self.clients {
                    if other == peer {
                        continue;
                    }
                    if let Some(targets) = message["targetClientIds"].as_array() {
                        if !targets.iter().any(|target| target == &client.id) {
                            continue;
                        }
                    }
                    output.push((other, message.clone()));
                }
            }
            _ => {}
        }
        output
    }
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
    fn status(&self, peer: Peer, status: &str) -> Outgoing {
        let Some(client) = self.clients.get(&peer) else {
            return vec![];
        };
        let message = json!({"type":"broadcast", "method":"client-status-changed", "sourceClientId":client.id,
            "version":0, "params":{"clientId":client.id, "clientType":client.kind, "status":status}});
        self.clients
            .keys()
            .filter(|&&other| other != peer)
            .map(|&other| (other, message.clone()))
            .collect()
    }
    pub fn disconnect(&mut self, peer: Peer) -> Outgoing {
        let mut output = if self.clients.contains_key(&peer) {
            self.status(peer, "disconnected")
        } else {
            vec![]
        };
        self.clients.remove(&peer);
        self.pending.retain(|_, p| {
            if p.source == peer {
                return false;
            }
            if p.target == Some(peer)
                || p.discoveries.values().any(|&v| v == peer)
                || p.willing.contains(&peer)
            {
                output.push((p.source, protocol::error(&p.request, "client-disconnected")));
                return false;
            }
            true
        });
        output
    }
    pub fn expire(&mut self, now: Instant) -> Outgoing {
        let mut output = vec![];
        self.pending.retain(|_, p| {
            if p.deadline > now {
                return true;
            }
            output.push((p.source, protocol::error(&p.request, "request-timeout")));
            false
        });
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn initialize(router: &mut Router, peer: Peer) -> String {
        let out = router.message(peer, json!({"type":"request","requestId":"init","method":"initialize","params":{"clientType":"test"}}));
        out.last().unwrap().1["result"]["clientId"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    fn request() -> Value {
        json!({"type":"request","requestId":"same-id","method":"ide-context","version":0,"params":{"workspaceRoot":"/repo"}})
    }
    fn answer(router: &mut Router, message: &(Peer, Value), willing: bool) -> Outgoing {
        router.message(message.0, json!({"type":"client-discovery-response","requestId":message.1["requestId"],"response":{"canHandle":willing}}))
    }
    #[test]
    fn registration_discovery_forwarding_and_spoof_resistance() {
        let mut r = Router::default();
        let id = initialize(&mut r, 1);
        assert_eq!(initialize(&mut r, 1), id);
        let d = r.message(10, request());
        let forwarded = answer(&mut r, &d[0], true);
        assert_eq!(forwarded[0].0, 1);
        let response = protocol::success(&forwarded[0].1, &id, json!({"ideContext":{}}));
        assert!(r.message(99, response.clone()).is_empty());
        let result = r.message(1, response);
        assert_eq!(result[0].0, 10);
        assert_eq!(result[0].1["requestId"], "same-id");
    }
    #[test]
    fn duplicate_providers_are_rejected_in_either_order() {
        let mut r = Router::default();
        initialize(&mut r, 1);
        initialize(&mut r, 2);
        let d = r.message(10, request());
        assert!(answer(&mut r, &d[0], true).is_empty());
        assert_eq!(
            answer(&mut r, &d[1], true)[0].1["error"],
            "ambiguous-ide-provider"
        );
    }
    #[test]
    fn collisions_disconnect_timeout_and_no_provider() {
        let mut r = Router::default();
        assert_eq!(r.message(10, request())[0].1["error"], "no-client-found");
        initialize(&mut r, 1);
        let d1 = r.message(10, request());
        let d2 = r.message(11, request());
        let f1 = answer(&mut r, &d1[0], true);
        let f2 = answer(&mut r, &d2[0], true);
        assert_ne!(f1[0].1["requestId"], f2[0].1["requestId"]);
        let expired = r.expire(Instant::now() + Duration::from_secs(20));
        assert_eq!(expired.len(), 2);
        let d = r.message(10, request());
        answer(&mut r, &d[0], true);
        assert!(
            r.disconnect(1)
                .iter()
                .any(|(_, m)| m["error"] == "client-disconnected")
        );
        assert!(r.pending.is_empty());
    }
    #[test]
    fn forwards_unknown_methods_and_targeted_broadcasts() {
        let mut r = Router::default();
        initialize(&mut r, 1);
        let target = initialize(&mut r, 2);
        let mut req = request();
        req["method"] = json!("future-method");
        req["targetClientId"] = json!(target);
        let d = r.message(1, req);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].0, 2);
        assert_eq!(answer(&mut r, &d[0], true)[0].1["method"], "future-method");
        let out = r.message(1, json!({"type":"broadcast","method":"future-event","sourceClientId":"spoof","targetClientIds":[target]}));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, 2);
        assert_ne!(out[0].1["sourceClientId"], "spoof");
    }
}
