use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentStatus {
    Active,
    Waiting,
    Retired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentRole {
    Coordinator,
    Worker,
    Verifier,
    Monitor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Agent {
    /// id curto, usado em menções: @ui-worker -> id "ui-worker"
    pub id: String,
    /// nome exibido, ex: "@ui-worker"
    pub name: String,
    pub role: AgentRole,
    /// modelo de IA em uso, ex: "gpt-4o-mini"
    pub model: String,
    pub status: AgentStatus,
    /// tarefa atual (resumo do gatilho que acordou o agente)
    pub task: Option<String>,
    pub last_seen: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BoardPostKind {
    Message,
    System,
    ApprovalRequest,
    Alert,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardPost {
    pub id: String,
    /// id do agente que postou ("user" para o humano)
    pub agent_id: String,
    pub text: String,
    /// ids de agentes detectados via @menção no texto
    pub mentions: Vec<String>,
    pub ts: u64,
    pub kind: BoardPostKind,
}

/// Indicador visual exibido no Monaco quando um agente está editando um arquivo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEditIndicator {
    pub path: String,
    pub agent_id: String,
    pub ts: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequest {
    pub id: String,
    pub agent_id: String,
    /// descrição legível da ação pendente, ex: "Sobrescrever server/package.json"
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub status: ApprovalStatus,
    pub ts: u64,
}

// ---------------------------------------------------------------------------
// Protocolo WebSocket
// ---------------------------------------------------------------------------

/// Eventos client -> server
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ClientEvent {
    #[serde(rename = "task:submit")]
    TaskSubmit { prompt: String },
    #[serde(rename = "board:post")]
    BoardPost {
        text: String,
        #[serde(default)]
        from: Option<String>,
    },
    #[serde(rename = "approval:respond", rename_all = "camelCase")]
    ApprovalRespond { request_id: String, approved: bool },
    #[serde(rename = "agent:log", rename_all = "camelCase")]
    AgentLog { agent_id: String },
}

/// Eventos server -> client (broadcast)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServerEvent {
    #[serde(rename = "agents")]
    Agents { agents: Vec<Agent> },
    #[serde(rename = "board:history")]
    BoardHistory { posts: Vec<BoardPost> },
    #[serde(rename = "board:post")]
    BoardPost { post: BoardPost },
    #[serde(rename = "edit:indicator")]
    EditIndicator { indicator: FileEditIndicator },
    #[serde(rename = "edit:clear", rename_all = "camelCase")]
    EditClear { agent_id: String },
    #[serde(rename = "approval:request")]
    ApprovalRequest { request: ApprovalRequest },
    #[serde(rename = "agent:log", rename_all = "camelCase")]
    AgentLog {
        agent_id: String,
        lines: Vec<String>,
    },
    #[serde(rename = "status")]
    Status { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_events_match_wire_format() {
        let evt: ClientEvent = serde_json::from_str(
            r#"{"type":"approval:respond","requestId":"req-1","approved":true}"#,
        )
        .expect("parse approval:respond");
        assert_eq!(
            evt,
            ClientEvent::ApprovalRespond {
                request_id: "req-1".to_string(),
                approved: true
            }
        );

        let evt2: ClientEvent =
            serde_json::from_str(r#"{"type":"task:submit","prompt":"criar api"}"#)
                .expect("parse task:submit");
        assert_eq!(
            evt2,
            ClientEvent::TaskSubmit {
                prompt: "criar api".to_string()
            }
        );

        let evt3: ClientEvent =
            serde_json::from_str(r#"{"type":"board:post","text":"oi @all"}"#)
                .expect("parse board:post");
        assert_eq!(
            evt3,
            ClientEvent::BoardPost {
                text: "oi @all".to_string(),
                from: None
            }
        );

        let evt4: ClientEvent =
            serde_json::from_str(r#"{"type":"agent:log","agentId":"ui-worker"}"#)
                .expect("parse agent:log");
        assert_eq!(
            evt4,
            ClientEvent::AgentLog {
                agent_id: "ui-worker".to_string()
            }
        );
    }

    #[test]
    fn server_events_match_wire_format() {
        let clear = ServerEvent::EditClear {
            agent_id: "ui-worker".to_string(),
        };
        let v = serde_json::to_value(&clear).expect("serialize edit:clear");
        assert_eq!(
            v,
            serde_json::json!({"type": "edit:clear", "agentId": "ui-worker"})
        );

        let agent = Agent {
            id: "ui-worker".to_string(),
            name: "@ui-worker".to_string(),
            role: AgentRole::Worker,
            model: "gpt-4o-mini".to_string(),
            status: AgentStatus::Waiting,
            task: None,
            last_seen: 1234,
        };
        let agents_evt = ServerEvent::Agents {
            agents: vec![agent],
        };
        let av = serde_json::to_value(&agents_evt).expect("serialize agents");
        assert_eq!(av["type"], "agents");
        assert_eq!(av["agents"][0]["lastSeen"], 1234);
        assert!(av["agents"][0]["task"].is_null());
        assert_eq!(av["agents"][0]["status"], "waiting");
        assert_eq!(av["agents"][0]["role"], "worker");

        let post = BoardPost {
            id: "p1".to_string(),
            agent_id: "monitor".to_string(),
            text: "alert".to_string(),
            mentions: vec!["coordinator".to_string()],
            ts: 99,
            kind: BoardPostKind::ApprovalRequest,
        };
        let pv = serde_json::to_value(&ServerEvent::BoardPost { post }).unwrap();
        assert_eq!(pv["type"], "board:post");
        assert_eq!(pv["post"]["agentId"], "monitor");
        assert_eq!(pv["post"]["kind"], "approval-request");
    }
}
