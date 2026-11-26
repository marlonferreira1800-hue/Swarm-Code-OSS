use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::future::join_all;
use tokio::sync::{broadcast, oneshot, RwLock};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::llm::{LLMInput, LLMProvider};
use crate::types::{
    Agent, AgentRole, AgentStatus, ApprovalRequest, ApprovalStatus, BoardPost, BoardPostKind,
    ClientEvent, FileEditIndicator, ServerEvent,
};

pub const MAX_DEPTH: usize = 6;
pub const MAX_HISTORY: usize = 500;
pub const MONITOR_INTERVAL_MS: u64 = 5_000;
pub const MONITOR_STALE_MS: u64 = 30_000;

struct PendingApproval {
    request: ApprovalRequest,
    tx: oneshot::Sender<bool>,
}

pub struct SwarmEngine {
    llm: Arc<dyn LLMProvider>,
    agents: RwLock<Vec<Agent>>,
    posts: RwLock<VecDeque<BoardPost>>,
    pending_approvals: Mutex<HashMap<String, PendingApproval>>,
    event_tx: broadcast::Sender<ServerEvent>,
    monitor_handle: Mutex<Option<JoinHandle<()>>>,
    last_activity_at: AtomicU64,
    open_task: RwLock<Option<String>>,
    task_done: AtomicBool,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl SwarmEngine {
    pub fn new(llm: Arc<dyn LLMProvider>) -> Arc<Self> {
        let (event_tx, _) = broadcast::channel(512);
        Arc::new(Self {
            llm,
            agents: RwLock::new(Vec::new()),
            posts: RwLock::new(VecDeque::new()),
            pending_approvals: Mutex::new(HashMap::new()),
            event_tx,
            monitor_handle: Mutex::new(None),
            last_activity_at: AtomicU64::new(now_ms()),
            open_task: RwLock::new(None),
            task_done: AtomicBool::new(true),
        })
    }

    // -------------------------------------------------------------------------
    // Eventos / transporte
    // -------------------------------------------------------------------------

    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.event_tx.subscribe()
    }

    fn emit(&self, evt: ServerEvent) {
        let _ = self.event_tx.send(evt);
    }

    pub async fn get_agents(&self) -> Vec<Agent> {
        self.agents.read().await.clone()
    }

    pub async fn get_history(&self) -> Vec<BoardPost> {
        self.posts.read().await.iter().cloned().collect()
    }

    async fn broadcast_agents(&self) {
        let agents = self.get_agents().await;
        self.emit(ServerEvent::Agents { agents });
    }

    // -------------------------------------------------------------------------
    // Registro de agentes
    // -------------------------------------------------------------------------

    pub async fn register_agent(
        &self,
        id: impl Into<String>,
        role: AgentRole,
        model: impl Into<String>,
        task: impl Into<String>,
    ) -> Agent {
        let id = id.into();
        let agent = Agent {
            name: format!("@{id}"),
            id,
            role,
            model: model.into(),
            status: AgentStatus::Waiting,
            task: Some(task.into()),
            last_seen: now_ms(),
        };
        {
            let mut agents = self.agents.write().await;
            if let Some(existing) = agents.iter_mut().find(|a| a.id == agent.id) {
                *existing = agent.clone();
            } else {
                agents.push(agent.clone());
            }
        }
        self.broadcast_agents().await;
        agent
    }

    /// Agentes padrão do MVP.
    pub async fn register_default_agents(&self) {
        let model = std::env::var("SWARM_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());
        self.register_agent(
            "coordinator",
            AgentRole::Coordinator,
            &model,
            "Definir briefing e dividir subtarefas",
        )
        .await;
        self.register_agent(
            "ui-worker",
            AgentRole::Worker,
            &model,
            "Edição de UI / frontend",
        )
        .await;
        self.register_agent(
            "api-worker",
            AgentRole::Worker,
            &model,
            "Edição de backend / orquestrador",
        )
        .await;
        self.register_agent(
            "verifier",
            AgentRole::Verifier,
            &model,
            "Validar diffs e concluir a tarefa",
        )
        .await;
        self.register_agent(
            "monitor",
            AgentRole::Monitor,
            &model,
            "Watchdog de inatividade do enxame",
        )
        .await;
    }

    // -------------------------------------------------------------------------
    // Entrada de eventos do client
    // -------------------------------------------------------------------------

    pub async fn handle_client_event(self: &Arc<Self>, evt: ClientEvent) {
        match evt {
            ClientEvent::TaskSubmit { prompt } => {
                self.submit_task(prompt).await;
            }
            ClientEvent::BoardPost { text, from } => {
                let author = from.unwrap_or_else(|| "user".to_string());
                self.post(&author, &text, BoardPostKind::Message, 0).await;
            }
            ClientEvent::ApprovalRespond {
                request_id,
                approved,
            } => {
                self.respond_approval(&request_id, approved);
            }
            ClientEvent::AgentLog { .. } => {}
        }
    }

    /// Usuário envia uma tarefa: publica no mural e acorda o @coordinator.
    pub async fn submit_task(self: &Arc<Self>, prompt: String) {
        *self.open_task.write().await = Some(prompt.clone());
        self.task_done.store(false, Ordering::SeqCst);
        self.last_activity_at.store(now_ms(), Ordering::SeqCst);

        let trigger = BoardPost {
            id: Uuid::new_v4().to_string(),
            agent_id: "user".to_string(),
            text: format!("Nova tarefa: {prompt}"),
            mentions: vec!["coordinator".to_string()],
            ts: now_ms(),
            kind: BoardPostKind::System,
        };
        self.push_post(trigger.clone()).await;
        self.wake("coordinator", trigger, 0).await;
    }

    // -------------------------------------------------------------------------
    // Board (pub/sub por menções)
    // -------------------------------------------------------------------------

    /// Publica uma mensagem no mural e acorda todos os agentes mencionados.
    /// `@all` acorda o enxame inteiro (exceto quem postou).
    pub fn post<'a>(
        self: &'a Arc<Self>,
        agent_id: &'a str,
        text: &'a str,
        kind: BoardPostKind,
        depth: usize,
    ) -> Pin<Box<dyn Future<Output = BoardPost> + Send + 'a>> {
        Box::pin(async move {
            let mentions = self.parse_mentions(text).await;
            let post = BoardPost {
                id: Uuid::new_v4().to_string(),
                agent_id: agent_id.to_string(),
                text: text.to_string(),
                mentions: mentions.clone(),
                ts: now_ms(),
                kind,
            };
            self.push_post(post.clone()).await;

            let targets: Vec<String> = if mentions.iter().any(|m| m == "all") {
                self.agents
                    .read()
                    .await
                    .iter()
                    .filter(|a| a.id != agent_id)
                    .map(|a| a.id.clone())
                    .collect()
            } else {
                mentions
            };

            if depth < MAX_DEPTH && !targets.is_empty() {
                let futs = targets.into_iter().map(|target_id| {
                    let engine = Arc::clone(self);
                    let trigger = post.clone();
                    async move {
                        engine.wake(&target_id, trigger, depth).await;
                    }
                });
                join_all(futs).await;
            }

            post
        })
    }

    async fn push_post(&self, post: BoardPost) {
        {
            let mut posts = self.posts.write().await;
            posts.push_back(post.clone());
            while posts.len() > MAX_HISTORY {
                posts.pop_front();
            }
        }
        self.last_activity_at.store(now_ms(), Ordering::SeqCst);
        self.emit(ServerEvent::BoardPost { post });
    }

    async fn parse_mentions(&self, text: &str) -> Vec<String> {
        let known_ids: Vec<String> = self
            .agents
            .read()
            .await
            .iter()
            .map(|a| a.id.clone())
            .collect();

        let mut found = Vec::new();
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'@' {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric()
                        || bytes[end] == b'_'
                        || bytes[end] == b'-')
                {
                    end += 1;
                }
                if end > start {
                    let id = text[start..end].to_ascii_lowercase();
                    if (id == "all" || known_ids.iter().any(|k| k == &id))
                        && !found.iter().any(|f| f == &id)
                    {
                        found.push(id);
                    }
                    i = end;
                    continue;
                }
            }
            i += 1;
        }
        found
    }

    // -------------------------------------------------------------------------
    // Ciclo de vida do agente: waiting -> active -> (turno no LLM) -> waiting
    // -------------------------------------------------------------------------

    fn wake<'a>(
        self: &'a Arc<Self>,
        agent_id: &'a str,
        trigger: BoardPost,
        depth: usize,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let active_agent = {
                let mut agents = self.agents.write().await;
                let Some(agent) = agents.iter_mut().find(|a| a.id == agent_id) else {
                    return;
                };
                if agent.status != AgentStatus::Waiting {
                    return;
                }
                agent.status = AgentStatus::Active;
                agent.task = Some(trigger.text.chars().take(120).collect());
                agent.last_seen = now_ms();
                agent.clone()
            };
            self.broadcast_agents().await;

            self.run_turn(active_agent, trigger, depth).await;

            // Cleanup equivalente ao bloco `finally`
            self.emit(ServerEvent::EditClear {
                agent_id: agent_id.to_string(),
            });

            let transitioned = {
                let mut agents = self.agents.write().await;
                if let Some(agent) = agents.iter_mut().find(|a| a.id == agent_id) {
                    if agent.status == AgentStatus::Active {
                        agent.status = AgentStatus::Waiting;
                        agent.task = None;
                        agent.last_seen = now_ms();
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            };
            if transitioned {
                self.broadcast_agents().await;
            }
        })
    }

    async fn run_turn(self: &Arc<Self>, agent: Agent, trigger: BoardPost, depth: usize) {
        let context = {
            let posts = self.posts.read().await;
            let len = posts.len();
            let skip = len.saturating_sub(30);
            posts.iter().skip(skip).cloned().collect::<Vec<_>>()
        };
        let open_task = self.open_task.read().await.clone();

        let action = self
            .llm
            .complete(LLMInput {
                agent: agent.clone(),
                trigger,
                context,
                open_task,
            })
            .await;

        for text in action.posts {
            self.post(&agent.id, &text, BoardPostKind::Message, depth + 1)
                .await;
        }

        for edit in action.edits {
            let indicator = FileEditIndicator {
                path: edit.path.clone(),
                agent_id: agent.id.clone(),
                ts: now_ms(),
            };
            self.emit(ServerEvent::EditIndicator { indicator });
            self.emit(ServerEvent::AgentLog {
                agent_id: agent.id.clone(),
                lines: vec![format!("✏️ editando {} — {}", edit.path, edit.note)],
            });
        }

        if let Some(approval) = action.approval {
            {
                let mut agents = self.agents.write().await;
                if let Some(entry) = agents.iter_mut().find(|a| a.id == agent.id) {
                    entry.task = Some(format!("⏸ Aguardando aprovação: {}", approval.action));
                }
            }
            self.broadcast_agents().await;

            let approved = self
                .request_approval(
                    &agent.id,
                    approval.action.clone(),
                    approval.file_path,
                    approval.command,
                )
                .await;

            let msg = if approved {
                format!("✅ Aprovado pelo humano: {}", approval.action)
            } else {
                format!("⛔ Rejeitado pelo humano: {}", approval.action)
            };
            self.post(&agent.id, &msg, BoardPostKind::System, depth + 1)
                .await;
        }

        if action.complete_task {
            self.task_done.store(true, Ordering::SeqCst);
            *self.open_task.write().await = None;
            self.emit(ServerEvent::Status {
                message: "Tarefa concluída e verificada.".to_string(),
            });
        }

        if action.retire {
            {
                let mut agents = self.agents.write().await;
                if let Some(entry) = agents.iter_mut().find(|a| a.id == agent.id) {
                    entry.status = AgentStatus::Retired;
                }
            }
            self.broadcast_agents().await;
        }
    }

    // -------------------------------------------------------------------------
    // Aprovação human-in-the-loop
    // -------------------------------------------------------------------------

    pub async fn request_approval(
        self: &Arc<Self>,
        agent_id: &str,
        action: String,
        file_path: Option<String>,
        command: Option<String>,
    ) -> bool {
        let (tx, rx) = oneshot::channel();
        let request = ApprovalRequest {
            id: Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            action: action.clone(),
            file_path: file_path.clone(),
            command: command.clone(),
            status: ApprovalStatus::Pending,
            ts: now_ms(),
        };

        {
            let mut pending = self.pending_approvals.lock().unwrap();
            pending.insert(
                request.id.clone(),
                PendingApproval {
                    request: request.clone(),
                    tx,
                },
            );
        }

        self.emit(ServerEvent::ApprovalRequest { request });

        let mut msg = format!("⏸ Aguardando aprovação humana: {action}");
        if let Some(fp) = file_path {
            msg.push_str(&format!(" ({fp})"));
        }
        if let Some(cmd) = command {
            msg.push_str(&format!(" — comando: `{cmd}`"));
        }
        self.post(agent_id, &msg, BoardPostKind::ApprovalRequest, MAX_DEPTH)
            .await;

        rx.await.unwrap_or(false)
    }

    pub fn respond_approval(&self, request_id: &str, approved: bool) {
        let pending = {
            let mut map = self.pending_approvals.lock().unwrap();
            map.remove(request_id)
        };
        if let Some(mut item) = pending {
            item.request.status = if approved {
                ApprovalStatus::Approved
            } else {
                ApprovalStatus::Rejected
            };
            self.emit(ServerEvent::ApprovalRequest {
                request: item.request,
            });
            let _ = item.tx.send(approved);
        }
    }

    // -------------------------------------------------------------------------
    // Watchdog (@monitor)
    // -------------------------------------------------------------------------

    pub fn start_monitor(self: &Arc<Self>, interval_ms: u64, stale_ms: u64) {
        let mut guard = self.monitor_handle.lock().unwrap();
        if guard.is_some() {
            return;
        }
        let engine = Arc::clone(self);
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms));
            ticker.tick().await; // descarta tick imediato
            loop {
                ticker.tick().await;
                if engine.task_done.load(Ordering::SeqCst) {
                    continue;
                }
                let stale_for = now_ms().saturating_sub(engine.last_activity_at.load(Ordering::SeqCst));
                if stale_for < stale_ms {
                    continue;
                }
                let monitor_alive = {
                    let agents = engine.agents.read().await;
                    agents
                        .iter()
                        .find(|a| a.id == "monitor")
                        .is_some_and(|a| a.status != AgentStatus::Retired)
                };
                if !monitor_alive {
                    continue;
                }
                let secs = (stale_for as f64 / 1000.0).round() as u64;
                engine
                    .post(
                        "monitor",
                        &format!(
                            "⚠️ Alerta do watchdog: enxame inativo há {secs}s e a tarefa ainda não foi concluída. @coordinator, retome o controle."
                        ),
                        BoardPostKind::Alert,
                        0,
                    )
                    .await;
            }
        });
        *guard = Some(handle);
    }

    pub fn stop(&self) {
        let mut guard = self.monitor_handle.lock().unwrap();
        if let Some(handle) = guard.take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{LLMAction, LLMApproval, LLMEdit};
    use async_trait::async_trait;
    use std::sync::atomic::AtomicUsize;

    struct FnLLM<F>(F);

    #[async_trait]
    impl<F, Fut> LLMProvider for FnLLM<F>
    where
        F: Fn(LLMInput) -> Fut + Send + Sync,
        Fut: Future<Output = LLMAction> + Send,
    {
        async fn complete(&self, input: LLMInput) -> LLMAction {
            (self.0)(input).await
        }
    }

    #[tokio::test]
    async fn wakes_all_non_author_agents_on_at_all() {
        let woken = Arc::new(Mutex::new(Vec::<String>::new()));
        let woken_ref = Arc::clone(&woken);
        let llm: Arc<dyn LLMProvider> = Arc::new(FnLLM(move |input: LLMInput| {
            let woken_ref = Arc::clone(&woken_ref);
            async move {
                woken_ref.lock().unwrap().push(input.agent.id);
                LLMAction::default()
            }
        }));

        let engine = SwarmEngine::new(llm);
        engine.register_default_agents().await;

        let post = engine
            .post(
                "coordinator",
                "Atenção @all, sincronizem o status.",
                BoardPostKind::Message,
                0,
            )
            .await;
        assert_eq!(post.mentions, vec!["all"]);

        let mut list = woken.lock().unwrap().clone();
        list.sort();
        assert_eq!(
            list,
            vec!["api-worker", "monitor", "ui-worker", "verifier"]
        );
    }

    #[tokio::test]
    async fn wakes_multiple_mentioned_workers_concurrently() {
        let active_now = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let seen_both_active = Arc::new(AtomicBool::new(false));
        let engine_slot: Arc<Mutex<Option<Arc<SwarmEngine>>>> = Arc::new(Mutex::new(None));

        let a_ref = Arc::clone(&active_now);
        let m_ref = Arc::clone(&max_concurrent);
        let b_ref = Arc::clone(&seen_both_active);
        let e_ref = Arc::clone(&engine_slot);

        let llm: Arc<dyn LLMProvider> = Arc::new(FnLLM(move |input: LLMInput| {
            let a_ref = Arc::clone(&a_ref);
            let m_ref = Arc::clone(&m_ref);
            let b_ref = Arc::clone(&b_ref);
            let e_ref = Arc::clone(&e_ref);
            async move {
                let cur = a_ref.fetch_add(1, Ordering::SeqCst) + 1;
                m_ref.fetch_max(cur, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(40)).await;

                let eng = e_ref.lock().unwrap().clone();
                if let Some(eng) = eng {
                    let agents = eng.get_agents().await;
                    let ui_active = agents
                        .iter()
                        .any(|a| a.id == "ui-worker" && a.status == AgentStatus::Active);
                    let api_active = agents
                        .iter()
                        .any(|a| a.id == "api-worker" && a.status == AgentStatus::Active);
                    if ui_active && api_active {
                        b_ref.store(true, Ordering::SeqCst);
                    }
                }

                a_ref.fetch_sub(1, Ordering::SeqCst);
                LLMAction {
                    posts: vec![format!("{} concluído", input.agent.name)],
                    ..Default::default()
                }
            }
        }));

        let engine = SwarmEngine::new(llm);
        *engine_slot.lock().unwrap() = Some(Arc::clone(&engine));
        engine.register_default_agents().await;

        engine
            .post(
                "coordinator",
                "Frentes: @ui-worker e @api-worker",
                BoardPostKind::Message,
                0,
            )
            .await;

        assert_eq!(max_concurrent.load(Ordering::SeqCst), 2);
        assert!(seen_both_active.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn emits_edit_clear_when_turn_completes() {
        let llm: Arc<dyn LLMProvider> = Arc::new(FnLLM(|_input: LLMInput| async {
            LLMAction {
                edits: vec![LLMEdit {
                    path: "src/app.ts".to_string(),
                    note: "atualizando componente".to_string(),
                }],
                ..Default::default()
            }
        }));

        let engine = SwarmEngine::new(llm);
        engine.register_default_agents().await;
        let mut rx = engine.subscribe();

        engine
            .post(
                "coordinator",
                "@ui-worker edite o arquivo",
                BoardPostKind::Message,
                0,
            )
            .await;

        let mut events = Vec::new();
        while let Ok(evt) = rx.try_recv() {
            events.push(evt);
        }

        let indicator_idx = events.iter().position(|e| {
            matches!(
                e,
                ServerEvent::EditIndicator { indicator } if indicator.agent_id == "ui-worker"
            )
        });
        let clear_idx = events.iter().position(|e| {
            matches!(
                e,
                ServerEvent::EditClear { agent_id } if agent_id == "ui-worker"
            )
        });

        assert!(indicator_idx.is_some());
        assert!(clear_idx.is_some());
        assert!(clear_idx.unwrap() > indicator_idx.unwrap());
    }

    #[tokio::test]
    async fn approval_pause_and_resume_approved_and_rejected() {
        let turn_count = Arc::new(AtomicUsize::new(0));
        let tc = Arc::clone(&turn_count);

        let llm: Arc<dyn LLMProvider> = Arc::new(FnLLM(move |_input: LLMInput| {
            let tc = Arc::clone(&tc);
            async move {
                let n = tc.fetch_add(1, Ordering::SeqCst) + 1;
                LLMAction {
                    approval: Some(LLMApproval {
                        action: if n == 1 {
                            "Sobrescrever package.json".to_string()
                        } else {
                            "Apagar cache".to_string()
                        },
                        file_path: Some("server/package.json".to_string()),
                        command: None,
                    }),
                    ..Default::default()
                }
            }
        }));

        let engine = SwarmEngine::new(llm);
        engine.register_default_agents().await;
        let mut rx = engine.subscribe();

        // Fluxo aprovado
        let eng1 = Arc::clone(&engine);
        let first_post = tokio::spawn(async move {
            eng1.post(
                "coordinator",
                "@api-worker execute ação crítica",
                BoardPostKind::Message,
                0,
            )
            .await;
        });

        let req_id_1 = loop {
            match rx.recv().await.unwrap() {
                ServerEvent::ApprovalRequest { request }
                    if request.status == ApprovalStatus::Pending =>
                {
                    break request.id;
                }
                _ => {}
            }
        };

        let paused = engine
            .get_agents()
            .await
            .into_iter()
            .find(|a| a.id == "api-worker")
            .unwrap();
        assert_eq!(paused.status, AgentStatus::Active);
        assert_eq!(
            paused.task.as_deref(),
            Some("⏸ Aguardando aprovação: Sobrescrever package.json")
        );

        engine
            .handle_client_event(ClientEvent::ApprovalRespond {
                request_id: req_id_1.clone(),
                approved: true,
            })
            .await;
        first_post.await.unwrap();

        let mut approved_seen = false;
        while let Ok(evt) = rx.try_recv() {
            if let ServerEvent::ApprovalRequest { request } = evt {
                if request.id == req_id_1 && request.status == ApprovalStatus::Approved {
                    approved_seen = true;
                }
            }
        }
        assert!(approved_seen);
        assert!(engine
            .get_history()
            .await
            .iter()
            .any(|p| p.text.contains("✅ Aprovado pelo humano: Sobrescrever package.json")));

        let resumed = engine
            .get_agents()
            .await
            .into_iter()
            .find(|a| a.id == "api-worker")
            .unwrap();
        assert_eq!(resumed.status, AgentStatus::Waiting);
        assert_eq!(resumed.task, None);

        // Fluxo rejeitado
        let eng2 = Arc::clone(&engine);
        let second_post = tokio::spawn(async move {
            eng2.post(
                "coordinator",
                "@api-worker tente novamente",
                BoardPostKind::Message,
                0,
            )
            .await;
        });

        let req_id_2 = loop {
            match rx.recv().await.unwrap() {
                ServerEvent::ApprovalRequest { request }
                    if request.id != req_id_1 && request.status == ApprovalStatus::Pending =>
                {
                    break request.id;
                }
                _ => {}
            }
        };

        engine
            .handle_client_event(ClientEvent::ApprovalRespond {
                request_id: req_id_2.clone(),
                approved: false,
            })
            .await;
        second_post.await.unwrap();

        let mut rejected_seen = false;
        while let Ok(evt) = rx.try_recv() {
            if let ServerEvent::ApprovalRequest { request } = evt {
                if request.id == req_id_2 && request.status == ApprovalStatus::Rejected {
                    rejected_seen = true;
                }
            }
        }
        assert!(rejected_seen);
        assert!(engine
            .get_history()
            .await
            .iter()
            .any(|p| p.text.contains("⛔ Rejeitado pelo humano: Apagar cache")));
    }
}
