use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::types::{Agent, AgentRole, BoardPost};

#[derive(Debug, Clone)]
pub struct LLMInput {
    pub agent: Agent,
    /// post que acordou o agente
    pub trigger: BoardPost,
    /// últimas mensagens do mural (contexto)
    pub context: Vec<BoardPost>,
    pub open_task: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LLMEdit {
    pub path: String,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LLMApproval {
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LLMAction {
    #[serde(default)]
    pub posts: Vec<String>,
    #[serde(default)]
    pub edits: Vec<LLMEdit>,
    #[serde(default)]
    pub approval: Option<LLMApproval>,
    #[serde(default)]
    pub retire: bool,
    #[serde(default)]
    pub complete_task: bool,
}

#[async_trait]
pub trait LLMProvider: Send + Sync {
    async fn complete(&self, input: LLMInput) -> LLMAction;
}

/// Se `OPENAI_API_KEY` estiver configurada, usa `OpenAILLM` via `reqwest`.
/// Sem chave, cai no `SimulatedLLM`, que roda um fluxo determinístico para a demo.
pub fn create_llm_provider() -> Arc<dyn LLMProvider> {
    if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
        if !api_key.trim().is_empty() {
            let model =
                std::env::var("SWARM_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());
            return Arc::new(OpenAILLM::new(api_key, model));
        }
    }
    eprintln!("[llm] OPENAI_API_KEY ausente — rodando com SimulatedLLM (demo).");
    Arc::new(SimulatedLLM::new())
}

// ---------------------------------------------------------------------------
// OpenAILLM (produção via reqwest)
// ---------------------------------------------------------------------------

const SYSTEM_PROMPT: &str = r#"Você é um agente de um enxame (swarm) dentro de uma IDE.
Responda SEMPRE com um único objeto JSON (sem texto fora do JSON) no formato:
{
  "posts": ["mensagem para o mural, pode usar @nome para acordar outro agente"],
  "edits": [{ "path": "caminho/do/arquivo", "note": "descrição curta" }],
  "approval": { "action": "descrição", "filePath": "opcional", "command": "opcional" },
  "retire": false,
  "completeTask": false
}
Regras: só inclua os campos necessários; omita "posts" se não houver nada a dizer.
Para acordar outro agente use @id no texto (ex: @verifier). "completeTask" só quando a tarefa foi concluída de verdade."#;

pub struct OpenAILLM {
    client: reqwest::Client,
    api_key: String,
    model: String,
    endpoint: String,
}

#[derive(Serialize)]
struct OpenAIChatRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAIMessage>,
}

#[derive(Serialize)]
struct OpenAIMessage {
    role: &'static str,
    content: String,
}

#[derive(Deserialize)]
struct OpenAIChatResponse {
    choices: Vec<OpenAIChoice>,
}

#[derive(Deserialize)]
struct OpenAIChoice {
    message: OpenAIChoiceMessage,
}

#[derive(Deserialize)]
struct OpenAIChoiceMessage {
    content: Option<String>,
}

impl OpenAILLM {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        let base_url = std::env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
        let endpoint = format!("{}/chat/completions", base_url.trim_end_matches('/'));
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            model: model.into(),
            endpoint,
        }
    }
}

#[async_trait]
impl LLMProvider for OpenAILLM {
    async fn complete(&self, input: LLMInput) -> LLMAction {
        let context_text = input
            .context
            .iter()
            .map(|p| {
                let kind = serde_json::to_value(p.kind)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_else(|| "message".to_string());
                format!("{} [{}]: {}", p.agent_id, kind, p.text)
            })
            .collect::<Vec<_>>()
            .join("\n");

        let role = serde_json::to_value(input.agent.role)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "worker".to_string());

        let system = format!(
            "{SYSTEM_PROMPT}\n\nSeu papel: {role}. Seu id: {}.",
            input.agent.id
        );
        let user_prompt = format!(
            "Tarefa em aberto: {}\n\nMural (cronológico):\n{}\n\nGatilho que te acordou ({}): {}\n\nSua resposta JSON:",
            input.open_task.as_deref().unwrap_or("(nenhuma)"),
            context_text,
            input.trigger.agent_id,
            input.trigger.text
        );

        let body = OpenAIChatRequest {
            model: &self.model,
            messages: vec![
                OpenAIMessage {
                    role: "system",
                    content: system,
                },
                OpenAIMessage {
                    role: "user",
                    content: user_prompt,
                },
            ],
        };

        let resp = match self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(err) => {
                eprintln!("[llm] erro HTTP OpenAI: {err}");
                return LLMAction::default();
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            eprintln!("[llm] OpenAI retornou {status}: {text}");
            return LLMAction::default();
        }

        let parsed: OpenAIChatResponse = match resp.json().await {
            Ok(p) => p,
            Err(err) => {
                eprintln!("[llm] erro ao decodificar resposta OpenAI: {err}");
                return LLMAction::default();
            }
        };

        let content = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default();

        parse_action(&content)
    }
}

pub fn parse_action(text: &str) -> LLMAction {
    let trimmed = text.trim();

    // Tenta extrair bloco ```json ... ``` ou ``` ... ```
    if let Some(start_fence) = trimmed.find("```") {
        let after_fence = &trimmed[start_fence + 3..];
        let body = after_fence
            .strip_prefix("json")
            .unwrap_or(after_fence)
            .trim_start();
        if let Some(end_fence) = body.find("```") {
            let candidate = body[..end_fence].trim();
            if let Ok(action) = serde_json::from_str::<LLMAction>(candidate) {
                return action;
            }
        }
    }

    // Tenta extrair do primeiro '{' ao último '}'
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) {
        if start <= end {
            if let Ok(action) = serde_json::from_str::<LLMAction>(&trimmed[start..=end]) {
                return action;
            }
        }
    }

    LLMAction::default()
}

// ---------------------------------------------------------------------------
// SimulatedLLM (demo sem chave de API): fluxo determinístico por papel.
// ---------------------------------------------------------------------------

pub struct SimulatedLLM {
    turns: Mutex<HashMap<String, usize>>,
    delay_ms: u64,
}

impl Default for SimulatedLLM {
    fn default() -> Self {
        Self::new()
    }
}

impl SimulatedLLM {
    pub fn new() -> Self {
        let delay_ms = std::env::var("SWARM_SIM_DELAY_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(800);
        Self {
            turns: Mutex::new(HashMap::new()),
            delay_ms,
        }
    }

    pub fn with_delay(delay_ms: u64) -> Self {
        Self {
            turns: Mutex::new(HashMap::new()),
            delay_ms,
        }
    }
}

#[async_trait]
impl LLMProvider for SimulatedLLM {
    async fn complete(&self, input: LLMInput) -> LLMAction {
        if self.delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        }

        let n = {
            let mut turns = self.turns.lock().unwrap();
            let entry = turns.entry(input.agent.id.clone()).or_insert(0);
            *entry += 1;
            *entry
        };

        match input.agent.role {
            AgentRole::Coordinator => {
                let task = input
                    .open_task
                    .as_deref()
                    .unwrap_or(&input.trigger.text);
                LLMAction {
                    posts: vec![format!(
                        "📋 Briefing da tarefa: \"{task}\". Dividindo em frentes — @ui-worker cuida do painel (UI) e @api-worker cuida do orquestrador (API)."
                    )],
                    ..Default::default()
                }
            }
            AgentRole::Worker => {
                let is_ui = input.agent.id.contains("ui");
                let file = if is_ui {
                    "client/src/components/SwarmPanel/SwarmPanel.tsx"
                } else {
                    "server/src/engine/SwarmEngine.ts"
                };
                let mut action = LLMAction {
                    posts: vec![format!(
                        "🔧 {} editando `{}` e reportando no mural. @verifier, valide o diff quando eu terminar.",
                        input.agent.name, file
                    )],
                    edits: vec![LLMEdit {
                        path: file.to_string(),
                        note: format!("{} editando...", input.agent.name),
                    }],
                    ..Default::default()
                };
                if n == 1 && !is_ui {
                    action.approval = Some(LLMApproval {
                        action: "Sobrescrever arquivo crítico".to_string(),
                        file_path: Some("server/package.json".to_string()),
                        command: None,
                    });
                }
                action
            }
            AgentRole::Verifier => {
                if input.open_task.is_none() {
                    return LLMAction::default();
                }
                LLMAction {
                    posts: vec![
                        "✅ Verificação concluída: diffs consistentes, sem quebras aparentes. @coordinator pode encerrar a tarefa. @all bom trabalho.".to_string(),
                    ],
                    complete_task: true,
                    ..Default::default()
                }
            }
            AgentRole::Monitor => LLMAction::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AgentStatus, BoardPostKind};

    #[test]
    fn parse_action_handles_code_fence_and_raw_json() {
        let fenced = "Aqui está:\n```json\n{\"posts\":[\"ok @verifier\"],\"completeTask\":true}\n```";
        let a = parse_action(fenced);
        assert_eq!(a.posts, vec!["ok @verifier"]);
        assert!(a.complete_task);

        let raw = r#"{"edits":[{"path":"src/lib.rs","note":"edit"}]}"#;
        let b = parse_action(raw);
        assert_eq!(b.edits.len(), 1);
        assert_eq!(b.edits[0].path, "src/lib.rs");
    }

    #[tokio::test]
    async fn simulated_llm_briefs_workers_without_verifier_and_requests_approval_on_api_worker_turn_1() {
        let sim = SimulatedLLM::with_delay(0);
        let coord = Agent {
            id: "coordinator".into(),
            name: "@coordinator".into(),
            role: AgentRole::Coordinator,
            model: "gpt-4o-mini".into(),
            status: AgentStatus::Active,
            task: None,
            last_seen: 0,
        };
        let trigger = BoardPost {
            id: "1".into(),
            agent_id: "user".into(),
            text: "Nova tarefa: testar".into(),
            mentions: vec!["coordinator".into()],
            ts: 0,
            kind: BoardPostKind::System,
        };
        let coord_act = sim
            .complete(LLMInput {
                agent: coord,
                trigger: trigger.clone(),
                context: vec![],
                open_task: Some("testar".into()),
            })
            .await;

        let briefing = &coord_act.posts[0];
        assert!(briefing.contains("@ui-worker"));
        assert!(briefing.contains("@api-worker"));
        assert!(!briefing.contains("@verifier"));

        let api_worker = Agent {
            id: "api-worker".into(),
            name: "@api-worker".into(),
            role: AgentRole::Worker,
            model: "gpt-4o-mini".into(),
            status: AgentStatus::Active,
            task: None,
            last_seen: 0,
        };
        let api_act1 = sim
            .complete(LLMInput {
                agent: api_worker.clone(),
                trigger: trigger.clone(),
                context: vec![],
                open_task: Some("testar".into()),
            })
            .await;
        assert!(api_act1.approval.is_some());
        assert!(!api_act1.edits.is_empty());

        let api_act2 = sim
            .complete(LLMInput {
                agent: api_worker,
                trigger,
                context: vec![],
                open_task: Some("testar".into()),
            })
            .await;
        assert!(api_act2.approval.is_none());
    }
}
