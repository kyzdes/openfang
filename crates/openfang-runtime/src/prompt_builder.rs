//! Centralized system prompt builder.
//!
//! Assembles a structured, multi-section system prompt from agent context.
//! Replaces the scattered `push_str` prompt injection throughout the codebase
//! with a single, testable, ordered prompt builder.

use crate::str_utils::safe_truncate_str;

/// All the context needed to build a system prompt for an agent.
#[derive(Debug, Clone, Default)]
pub struct PromptContext {
    /// Agent name (from manifest).
    pub agent_name: String,
    /// Agent description (from manifest).
    pub agent_description: String,
    /// Base system prompt authored in the agent manifest.
    pub base_system_prompt: String,
    /// Tool names this agent has access to.
    pub granted_tools: Vec<String>,
    /// Recalled memories as (key, content) pairs.
    pub recalled_memories: Vec<(String, String)>,
    /// Skill summary text (from kernel.build_skill_summary()).
    pub skill_summary: String,
    /// Prompt context from prompt-only skills.
    pub skill_prompt_context: String,
    /// MCP server/tool summary text.
    pub mcp_summary: String,
    /// Agent workspace path.
    pub workspace_path: Option<String>,
    /// SOUL.md content (persona).
    pub soul_md: Option<String>,
    /// USER.md content.
    pub user_md: Option<String>,
    /// MEMORY.md content.
    pub memory_md: Option<String>,
    /// Cross-channel canonical context summary.
    pub canonical_context: Option<String>,
    /// Known user name (from shared memory).
    pub user_name: Option<String>,
    /// Channel type (telegram, discord, web, etc.).
    pub channel_type: Option<String>,
    /// Whether this agent was spawned as a subagent.
    pub is_subagent: bool,
    /// Whether this agent has autonomous config.
    pub is_autonomous: bool,
    /// AGENTS.md content (behavioral guidance).
    pub agents_md: Option<String>,
    /// BOOTSTRAP.md content (first-run ritual).
    pub bootstrap_md: Option<String>,
    /// Workspace context section (project type, context files).
    pub workspace_context: Option<String>,
    /// IDENTITY.md content (visual identity + personality frontmatter).
    pub identity_md: Option<String>,
    /// HEARTBEAT.md content (autonomous agent checklist).
    pub heartbeat_md: Option<String>,
    /// Peer agents visible to this agent: (name, state, model).
    pub peer_agents: Vec<(String, String, String)>,
    /// Current date/time string for temporal awareness.
    pub current_date: Option<String>,
    /// Sender identity (e.g. WhatsApp phone number, Telegram user ID).
    pub sender_id: Option<String>,
    /// Sender display name.
    pub sender_name: Option<String>,
    /// Current on-disk `context.md` content for the agent (see `agent_context`).
    ///
    /// Read per-turn by the kernel so external writers (cron jobs, integrations)
    /// are reflected in the next LLM call. See issue #843.
    pub context_md: Option<String>,
}

/// Build the complete system prompt from a `PromptContext`.
///
/// Produces an ordered, multi-section prompt. Sections with no content are
/// omitted entirely (no empty sections). Subagent mode skips sections that
/// add unnecessary context overhead.
///
/// Runtime *data* sections are wrapped in XML-like tags (see [`data_section`]),
/// never `## ` markdown headings. Behavioral *instruction* prose (tool call
/// behavior, safety, operational guidelines, first-run, heartbeat) keeps its
/// markdown headings on purpose — see [`data_section`] for the reasoning.
pub fn build_system_prompt(ctx: &PromptContext) -> String {
    let mut sections: Vec<String> = Vec::with_capacity(12);

    // Section 1 — Agent Identity (always present)
    sections.push(build_identity_section(ctx));

    // Section 1.5 — Current Date/Time (always present when set)
    if let Some(ref date) = ctx.current_date {
        sections.push(data_section("current_date", &format!("Today is {date}.")));
    }

    // Section 2 — Tool Call Behavior (skip for subagents)
    if !ctx.is_subagent {
        sections.push(TOOL_CALL_BEHAVIOR.to_string());
    }

    // Section 2.5 — Agent Behavioral Guidelines (skip for subagents)
    if !ctx.is_subagent {
        if let Some(ref agents) = ctx.agents_md {
            if !agents.trim().is_empty() {
                sections.push(cap_str(agents, 2000));
            }
        }
    }

    // Section 3 — Available Tools (always present if tools exist)
    let tools_section = build_tools_section(&ctx.granted_tools);
    if !tools_section.is_empty() {
        sections.push(tools_section);
    }

    // Section 4 — Memory Protocol (always present)
    let mem_section = build_memory_section(&ctx.recalled_memories);
    sections.push(mem_section);

    // Section 5 — Skills (only if skills available)
    if !ctx.skill_summary.is_empty() || !ctx.skill_prompt_context.is_empty() {
        sections.push(build_skills_section(
            &ctx.skill_summary,
            &ctx.skill_prompt_context,
        ));
    }

    // Section 6 — MCP Servers (only if summary present)
    if !ctx.mcp_summary.is_empty() {
        sections.push(build_mcp_section(&ctx.mcp_summary));
    }

    // Section 7 — Persona / Identity files (skip for subagents)
    if !ctx.is_subagent {
        let persona = build_persona_section(
            ctx.identity_md.as_deref(),
            ctx.soul_md.as_deref(),
            ctx.user_md.as_deref(),
            ctx.memory_md.as_deref(),
            ctx.workspace_path.as_deref(),
        );
        if !persona.is_empty() {
            sections.push(persona);
        }
    }

    // Section 7.5 — Heartbeat checklist (only for autonomous agents)
    // Instruction prose — keeps its markdown heading (see `data_section`).
    if !ctx.is_subagent && ctx.is_autonomous {
        if let Some(ref heartbeat) = ctx.heartbeat_md {
            if !heartbeat.trim().is_empty() {
                sections.push(format!(
                    "## Heartbeat Checklist\n{}",
                    cap_str(heartbeat, 1000)
                ));
            }
        }
    }

    // Section 8 — User Personalization (skip for subagents)
    if !ctx.is_subagent {
        sections.push(build_user_section(ctx.user_name.as_deref()));
    }

    // Section 9 — Channel Awareness (skip for subagents)
    if !ctx.is_subagent {
        if let Some(ref channel) = ctx.channel_type {
            sections.push(build_channel_section(channel));
        }
    }

    // Section 9.1 — Sender Identity (skip for subagents)
    if !ctx.is_subagent {
        if let Some(sender_line) =
            build_sender_section(ctx.sender_name.as_deref(), ctx.sender_id.as_deref())
        {
            sections.push(sender_line);
        }
    }

    // Section 9.5 — Peer Agent Awareness (skip for subagents)
    if !ctx.is_subagent && !ctx.peer_agents.is_empty() {
        sections.push(build_peer_agents_section(&ctx.agent_name, &ctx.peer_agents));
    }

    // Section 10 — Safety & Oversight (skip for subagents)
    if !ctx.is_subagent {
        sections.push(SAFETY_SECTION.to_string());
    }

    // Section 11 — Operational Guidelines (always present)
    sections.push(OPERATIONAL_GUIDELINES.to_string());

    // Section 12 — Canonical Context moved to build_canonical_context_message()
    // to keep the system prompt stable across turns for provider prompt caching.

    // Section 13 — Bootstrap Protocol (only on first-run, skip for subagents)
    // Instruction prose — keeps its markdown heading (see `data_section`).
    if !ctx.is_subagent {
        if let Some(ref bootstrap) = ctx.bootstrap_md {
            if !bootstrap.trim().is_empty() {
                // Only inject if no user_name memory exists (first-run heuristic)
                let has_user_name = ctx.recalled_memories.iter().any(|(k, _)| k == "user_name");
                if !has_user_name && ctx.user_name.is_none() {
                    sections.push(format!(
                        "## First-Run Protocol\n{}",
                        cap_str(bootstrap, 1500)
                    ));
                }
            }
        }
    }

    // Section 14 — Workspace Context (skip for subagents)
    // Wrapped here rather than in `workspace_context.rs` so the closing tag
    // survives the cap below.
    if !ctx.is_subagent {
        if let Some(ref ws_ctx) = ctx.workspace_context {
            if !ws_ctx.trim().is_empty() {
                sections.push(data_section("workspace_context", &cap_file_blocks(ws_ctx, 1000)));
            }
        }
    }

    // Section 15 — Live agent context (`context.md`). Re-read per turn so
    // external writers (e.g. cron jobs refreshing live data) show up on the
    // very next message. See issue #843.
    if let Some(ref live) = ctx.context_md {
        let trimmed = live.trim();
        if !trimmed.is_empty() {
            sections.push(data_section(
                "live_context",
                &format!(
                    "The following context is refreshed from `context.md` each turn and may change between messages.\n\n{}",
                    cap_str(trimmed, 8000)
                ),
            ));
        }
    }

    sections.join("\n\n")
}

// ---------------------------------------------------------------------------
// Section builders
// ---------------------------------------------------------------------------

/// Wrap a runtime-data section in an XML-like tag instead of a `## ` heading.
///
/// An agent asked to write a markdown document treats the `## Section` blocks of
/// its own system prompt as part of the document and copies them into the
/// output: an AgentRAG2 report ended with a verbatim `## Current Date` block.
/// XML-like tags are not part of the markdown the agent is producing, so they
/// don't get echoed.
///
/// Applied to runtime *data* only (date, sender, channel, workspace, persona,
/// memory, tools, skills, peers, …). Behavioral instruction prose keeps its
/// markdown headings: heading structure is part of how strongly a model reads
/// text as a directive, and a regression in instruction-following costs more
/// than the leak being fixed here.
///
/// Only the wrapper changes — `body` reaches the model unchanged.
fn data_section(tag: &str, body: &str) -> String {
    format!("<{tag}>\n{}\n</{tag}>", body.trim_end())
}

fn build_identity_section(ctx: &PromptContext) -> String {
    if ctx.base_system_prompt.is_empty() {
        format!(
            "You are {}, an AI agent running inside the OpenFang Agent OS.\n{}",
            ctx.agent_name, ctx.agent_description
        )
    } else {
        ctx.base_system_prompt.clone()
    }
}

/// Static tool-call behavior directives.
///
/// Instruction prose — keeps its markdown heading (see [`data_section`]).
const TOOL_CALL_BEHAVIOR: &str = "\
## Tool Call Behavior
- When you need to use a tool, call it immediately. Do not narrate or explain routine tool calls.
- Only explain tool calls when the action is destructive, unusual, or the user explicitly asked for an explanation.
- Prefer action over narration. If you can answer by using a tool, do it.
- When executing multiple sequential tool calls, batch them — don't output reasoning between each call.
- If a tool returns useful results, present the KEY information, not the raw output.
- When web_fetch or web_search returns content, you MUST include the relevant data in your response. \
Quote specific facts, numbers, or passages from the fetched content. Never say you fetched something \
without sharing what you found.
- Start with the answer, not meta-commentary about how you'll help.
- IMPORTANT: If your instructions or persona mention a shell command, script path, or code snippet, \
execute it via the appropriate tool call (shell_exec, file_write, etc.). Never output commands as \
code blocks — always call the tool instead.";

/// Build the grouped tools section (Section 3).
pub fn build_tools_section(granted_tools: &[String]) -> String {
    if granted_tools.is_empty() {
        return String::new();
    }

    // Group tools by category
    let mut groups: std::collections::BTreeMap<&str, Vec<(&str, &str)>> =
        std::collections::BTreeMap::new();
    for name in granted_tools {
        let cat = tool_category(name);
        let hint = tool_hint(name);
        groups.entry(cat).or_default().push((name.as_str(), hint));
    }

    let mut out = String::from("You have access to these capabilities:\n");
    for (category, tools) in &groups {
        out.push_str(&format!("\n**{}**: ", capitalize(category)));
        let descs: Vec<String> = tools
            .iter()
            .map(|(name, hint)| {
                if hint.is_empty() {
                    (*name).to_string()
                } else {
                    format!("{name} ({hint})")
                }
            })
            .collect();
        out.push_str(&descs.join(", "));
    }
    data_section("your_tools", &out)
}

/// Build canonical context as a standalone user message (instead of system prompt).
///
/// This keeps the system prompt stable across turns, enabling provider prompt caching
/// (Anthropic cache_control, etc.). The canonical context changes every turn, so
/// injecting it in the system prompt caused 82%+ cache misses.
pub fn build_canonical_context_message(ctx: &PromptContext) -> Option<String> {
    if ctx.is_subagent {
        return None;
    }
    ctx.canonical_context
        .as_ref()
        .filter(|c| !c.is_empty())
        .map(|c| format!("[Previous conversation context]\n{}", cap_str(c, 500)))
}

/// Build the memory section (Section 4).
///
/// Also used by `agent_loop.rs` to append recalled memories after DB lookup.
pub fn build_memory_section(memories: &[(String, String)]) -> String {
    let mut out = String::new();
    if memories.is_empty() {
        out.push_str(
            "- When the user asks about something from a previous conversation, use memory_recall first.\n\
             - Store important preferences, decisions, and context with memory_store for future use.",
        );
    } else {
        out.push_str(
            "- Use the recalled memories below to inform your responses.\n\
             - Only call memory_recall if you need information not already shown here.\n\
             - Store important preferences, decisions, and context with memory_store for future use.",
        );
        out.push_str("\n\nRecalled memories:\n");
        for (key, content) in memories.iter().take(5) {
            let capped = cap_str(content, 500);
            if key.is_empty() {
                out.push_str(&format!("- {capped}\n"));
            } else {
                out.push_str(&format!("- [{key}] {capped}\n"));
            }
        }
    }
    data_section("memory", &out)
}

fn build_skills_section(skill_summary: &str, prompt_context: &str) -> String {
    let mut out = String::new();
    if !skill_summary.is_empty() {
        out.push_str(
            "You have installed skills. If a request matches a skill, use its tools directly.\n",
        );
        out.push_str(skill_summary.trim());
    }
    if !prompt_context.is_empty() {
        out.push('\n');
        out.push_str(&cap_str(prompt_context, 2000));
    }
    data_section("skills", &out)
}

fn build_mcp_section(mcp_summary: &str) -> String {
    data_section("connected_tool_servers_mcp", mcp_summary.trim())
}

fn build_persona_section(
    identity_md: Option<&str>,
    soul_md: Option<&str>,
    user_md: Option<&str>,
    memory_md: Option<&str>,
    workspace_path: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(ws) = workspace_path {
        parts.push(data_section("workspace", &format!("Workspace: {ws}")));
    }

    // Identity file (IDENTITY.md) — personality at a glance, before SOUL.md
    if let Some(identity) = identity_md {
        if !identity.trim().is_empty() {
            parts.push(data_section("identity", &cap_str(identity, 500)));
        }
    }

    if let Some(soul) = soul_md {
        if !soul.trim().is_empty() {
            let sanitized = strip_code_blocks(soul);
            parts.push(data_section(
                "persona",
                &format!(
                    "Embody this identity in your tone and communication style. Be natural, not stiff or generic.\n{}",
                    cap_str(&sanitized, 1000)
                ),
            ));
        }
    }

    if let Some(user) = user_md {
        if !user.trim().is_empty() {
            parts.push(data_section("user_context", &cap_str(user, 500)));
        }
    }

    if let Some(memory) = memory_md {
        if !memory.trim().is_empty() {
            parts.push(data_section("long_term_memory", &cap_str(memory, 500)));
        }
    }

    parts.join("\n\n")
}

fn build_user_section(user_name: Option<&str>) -> String {
    let body = match user_name {
        Some(name) => {
            format!(
                "The user's name is \"{name}\". Address them by name naturally \
                 when appropriate (greetings, farewells, etc.), but don't overuse it."
            )
        }
        None => "You don't know the user's name yet. On your FIRST reply in this conversation, \
             warmly introduce yourself by your agent name and ask what they'd like to be called. \
             Once they tell you, immediately use the `memory_store` tool with \
             key \"user_name\" and their name as the value so you remember it for future sessions. \
             Keep the introduction brief — don't let it overshadow their actual request."
            .to_string(),
    };
    data_section("user_profile", &body)
}

fn build_channel_section(channel: &str) -> String {
    let (limit, hints) = match channel {
        "telegram" => (
            "4096",
            "Use Telegram-compatible formatting (bold with *, code with `backticks`).",
        ),
        "discord" => (
            "2000",
            "Use Discord markdown. Split long responses across multiple messages if needed.",
        ),
        "slack" => (
            "4000",
            "Use Slack mrkdwn formatting (*bold*, _italic_, `code`).",
        ),
        "whatsapp" => (
            "4096",
            "Keep messages concise. WhatsApp has limited formatting.",
        ),
        "irc" => (
            "512",
            "Keep messages very short. No markdown — plain text only.",
        ),
        "matrix" => (
            "65535",
            "Matrix supports rich formatting. Use markdown freely.",
        ),
        "teams" => ("28000", "Use Teams-compatible markdown."),
        _ => ("4096", "Use markdown formatting where supported."),
    };
    data_section(
        "channel",
        &format!(
            "You are responding via {channel}. Keep messages under {limit} chars.\n\
             {hints}"
        ),
    )
}

fn build_sender_section(sender_name: Option<&str>, sender_id: Option<&str>) -> Option<String> {
    let body = match (sender_name, sender_id) {
        (Some(name), Some(id)) => format!("Message from: {name} ({id})"),
        (Some(name), None) => format!("Message from: {name}"),
        (None, Some(id)) => format!("Message from: {id}"),
        (None, None) => return None,
    };
    Some(data_section("sender", &body))
}

fn build_peer_agents_section(self_name: &str, peers: &[(String, String, String)]) -> String {
    let mut out = String::from(
        "You are part of a multi-agent system. These agents are running alongside you:\n",
    );
    for (name, state, model) in peers {
        if name == self_name {
            continue; // Don't list yourself
        }
        out.push_str(&format!("- **{}** ({}) — model: {}\n", name, state, model));
    }
    out.push_str(
        "\nYou can communicate with them using `agent_send` (by name) and see all agents with `agent_list`. \
         Delegate tasks to specialized agents when appropriate.",
    );
    data_section("peer_agents", &out)
}

/// Static safety section.
///
/// Instruction prose — keeps its markdown heading (see [`data_section`]).
const SAFETY_SECTION: &str = "\
## Safety
- Prioritize safety and human oversight over task completion.
- NEVER auto-execute purchases, payments, account deletions, or irreversible actions without explicit user confirmation.
- If a tool could cause data loss, explain what it will do and confirm first.
- If you cannot accomplish a task safely, explain the limitation.
- When in doubt, ask the user.";

/// Static operational guidelines (replaces STABILITY_GUIDELINES).
///
/// Instruction prose — keeps its markdown heading (see [`data_section`]).
const OPERATIONAL_GUIDELINES: &str = "\
## Operational Guidelines
- Do NOT retry a tool call with identical parameters if it failed. Try a different approach.
- If a tool returns an error, analyze the error before calling it again.
- Prefer targeted, specific tool calls over broad ones.
- Plan your approach before executing multiple tool calls.
- If you cannot accomplish a task after a few attempts, explain what went wrong instead of looping.
- Never call the same tool more than 3 times with the same parameters.
- If a message requires no response (simple acknowledgments, reactions, messages not directed at you), respond with exactly NO_REPLY.";

// ---------------------------------------------------------------------------
// Tool metadata helpers
// ---------------------------------------------------------------------------

/// Map a tool name to its category for grouping.
pub fn tool_category(name: &str) -> &'static str {
    match name {
        "file_read" | "file_write" | "file_list" | "file_delete" | "file_move" | "file_copy"
        | "file_search" => "Files",

        "web_search" | "web_fetch" => "Web",

        "browser_navigate" | "browser_click" | "browser_type" | "browser_screenshot"
        | "browser_read_page" | "browser_close" | "browser_scroll" | "browser_wait"
        | "browser_evaluate" | "browser_select" | "browser_back" => "Browser",

        "shell_exec" | "shell_background" => "Shell",

        "memory_store" | "memory_recall" | "memory_delete" | "memory_list" => "Memory",

        "agent_send" | "agent_spawn" | "agent_list" | "agent_kill" | "agent_activate" => "Agents",

        "image_describe" | "image_generate" | "audio_transcribe" | "tts_speak" => "Media",

        "docker_exec" | "docker_build" | "docker_run" => "Docker",

        "cron_create" | "cron_list" | "cron_delete" => "Scheduling",

        "process_start" | "process_poll" | "process_write" | "process_kill" | "process_list" => {
            "Processes"
        }

        _ if name.starts_with("mcp_") => "MCP",
        _ if name.starts_with("skill_") => "Skills",
        _ => "Other",
    }
}

/// Map a tool name to a one-line description hint.
pub fn tool_hint(name: &str) -> &'static str {
    match name {
        // Files
        "file_read" => "read file contents",
        "file_write" => "create or overwrite a file",
        "file_list" => "list directory contents",
        "file_delete" => "delete a file",
        "file_move" => "move or rename a file",
        "file_copy" => "copy a file",
        "file_search" => "search files by name pattern",

        // Web
        "web_search" => "search the web for information",
        "web_fetch" => "fetch a URL and get its content as markdown",

        // Browser
        "browser_navigate" => "open a URL in the browser",
        "browser_click" => "click an element on the page",
        "browser_type" => "type text into an input field",
        "browser_screenshot" => "capture a screenshot",
        "browser_read_page" => "extract page content as text",
        "browser_close" => "close the browser session",
        "browser_scroll" => "scroll the page",
        "browser_wait" => "wait for an element or condition",
        "browser_evaluate" => "run JavaScript on the page",
        "browser_select" => "select a dropdown option",
        "browser_back" => "go back to the previous page",

        // Shell
        "shell_exec" => "execute a shell command",
        "shell_background" => "run a command in the background",

        // Memory
        "memory_store" => "save a key-value pair to memory",
        "memory_recall" => "search memory for relevant context",
        "memory_delete" => "delete a memory entry",
        "memory_list" => "list stored memory keys",

        // Agents
        "agent_send" => "send a message to another agent",
        "agent_spawn" => "create a new agent",
        "agent_list" => "list running agents",
        "agent_kill" => "terminate an agent",
        "agent_activate" => "wake up an inactive agent so it can receive work",

        // Media
        "image_describe" => "describe an image",
        "image_generate" => "generate an image from a prompt",
        "audio_transcribe" => "transcribe audio to text",
        "tts_speak" => "convert text to speech",

        // Docker
        "docker_exec" => "run a command in a container",
        "docker_build" => "build a Docker image",
        "docker_run" => "start a Docker container",

        // Scheduling
        "cron_create" => "schedule a recurring task",
        "cron_list" => "list scheduled tasks",
        "cron_delete" => "remove a scheduled task",

        // Processes
        "process_start" => "start a long-running process (REPL, server)",
        "process_poll" => "read stdout/stderr from a running process",
        "process_write" => "write to a process's stdin",
        "process_kill" => "terminate a running process",
        "process_list" => "list active processes",

        _ => "",
    }
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

/// Cap a string to `max_chars`, appending "..." if truncated.
/// Strip markdown triple-backtick code blocks from content.
///
/// Prevents LLMs from copying code blocks as text output instead of making
/// tool calls when SOUL.md contains command examples.
fn strip_code_blocks(content: &str) -> String {
    let mut result = String::with_capacity(content.len());
    let mut in_block = false;
    for line in content.lines() {
        if line.trim_start().starts_with("```") {
            in_block = !in_block;
            continue;
        }
        if !in_block {
            result.push_str(line);
            result.push('\n');
        }
    }
    // Collapse multiple blank lines left by stripped blocks
    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }
    result.trim().to_string()
}

/// Cap the workspace-context body without ever cutting a `<file>` block open.
///
/// `cap_str` protects the outer `<workspace_context>` tag only, because the wrapper is
/// applied after it. The body itself is a list of `<file name="…">…</file>` blocks, and a
/// character cap lands inside one of them: measured on this box, 11 of 14 agents were
/// getting five opening tags and four closing ones. A dangling open tag is worse than the
/// `## ` heading this patch replaced — it invites the model to treat everything after it,
/// including its own answer, as file content.
///
/// So blocks are added whole and dropped whole. What is left out is stated rather than
/// silently missing, because an agent that cannot see a file at all should not conclude
/// the file is empty.
fn cap_file_blocks(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }

    // Group lines into units: a `<file …>` line opens a unit that runs to its `</file>`.
    let mut units: Vec<String> = Vec::new();
    let mut open: Option<String> = None;
    for line in s.lines() {
        match open.as_mut() {
            Some(buf) => {
                buf.push('\n');
                buf.push_str(line);
                if line.trim() == "</file>" {
                    units.push(open.take().unwrap());
                }
            }
            None => {
                if line.trim_start().starts_with("<file ") && line.trim() != "</file>" {
                    open = Some(line.to_string());
                } else {
                    units.push(line.to_string());
                }
            }
        }
    }
    // An unterminated block in the input is kept as-is: dropping it would hide data, and
    // the malformed tag came from upstream, not from this cap.
    if let Some(buf) = open.take() {
        units.push(buf);
    }

    let mut kept: Vec<String> = Vec::new();
    let mut dropped = 0usize;
    let mut used = 0usize;
    for unit in units {
        let extra = if kept.is_empty() { 0 } else { 1 };
        let len = unit.chars().count();
        if used + extra + len > max_chars {
            dropped += 1;
            continue;
        }
        used += extra + len;
        kept.push(unit);
    }

    // The omission note has to fit too, and it earns its place: an agent that cannot see a
    // file at all must not conclude the file is empty. If the budget is tight, give up kept
    // blocks for it rather than dropping the note — the note is what makes the gap legible.
    if dropped > 0 {
        loop {
            let note = format!("<omitted blocks=\"{dropped}\" reason=\"context budget\"/>");
            let extra = if kept.is_empty() { 0 } else { 1 };
            if used + extra + note.chars().count() <= max_chars {
                // `used` is deliberately not updated: nothing reads it after this break,
                // and clippy's unused-assignments is right to say so.
                kept.push(note);
                break;
            }
            match kept.pop() {
                Some(last) => {
                    used -= last.chars().count() + if kept.is_empty() { 0 } else { 1 };
                    dropped += 1;
                }
                // Nothing left to give up: the note alone exceeds the budget, so emit it
                // truncated rather than returning a body that hides the omission.
                None => {
                    kept.push(cap_str(&note, max_chars));
                    break;
                }
            }
        }
    }

    kept.join("\n")
}

fn cap_str(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let end = s
            .char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        safe_truncate_str(s, end).to_string() + "..."
    }
}

/// Capitalize the first letter of a string.
fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn basic_ctx() -> PromptContext {
        PromptContext {
            agent_name: "researcher".to_string(),
            agent_description: "Research agent".to_string(),
            base_system_prompt: "You are Researcher, a research agent.".to_string(),
            granted_tools: vec![
                "web_search".to_string(),
                "web_fetch".to_string(),
                "file_read".to_string(),
                "file_write".to_string(),
                "memory_store".to_string(),
                "memory_recall".to_string(),
            ],
            ..Default::default()
        }
    }

    /// Context with every runtime-data section populated. No fixture string
    /// contains a markdown heading, so every `## ` line in the built prompt
    /// comes from the builder itself.
    fn full_ctx() -> PromptContext {
        PromptContext {
            recalled_memories: vec![("pref".to_string(), "likes dark mode".to_string())],
            skill_summary: "- web-search: Search the web".to_string(),
            skill_prompt_context: "Prefer official docs.".to_string(),
            mcp_summary: "- github: 5 tools (search, create_issue)".to_string(),
            workspace_path: Some("/home/user/project".to_string()),
            soul_md: Some("Speak like a laconic engineer.".to_string()),
            user_md: Some("Runs a florist shop.".to_string()),
            memory_md: Some("Prefers metric units.".to_string()),
            identity_md: Some("Amber avatar, calm tone.".to_string()),
            heartbeat_md: Some("- Check the inbox every hour.".to_string()),
            is_autonomous: true,
            agents_md: Some("Answer in the user's language.".to_string()),
            bootstrap_md: Some("Introduce yourself once.".to_string()),
            workspace_context: Some(
                "- Project: openfang (Rust)\n<file name=\"SOUL.md\">\nBe nice\n</file>".to_string(),
            ),
            peer_agents: vec![(
                "archivist".to_string(),
                "running".to_string(),
                "glm-5.2".to_string(),
            )],
            current_date: Some("Tuesday, August 11, 2026 (2026-08-11 22:29 +00:00)".to_string()),
            sender_id: Some("79990001122".to_string()),
            sender_name: Some("Katya".to_string()),
            channel_type: Some("telegram".to_string()),
            context_md: Some("BTCUSD: 67000".to_string()),
            // user_name stays None so the first-run branch is exercised too.
            ..basic_ctx()
        }
    }

    /// Regression for the wild leak: an AgentRAG2 report ended with a verbatim
    /// `## Current Date` / `Today is …` block copied out of the system prompt.
    #[test]
    fn test_current_date_is_data_not_a_markdown_section() {
        let mut ctx = basic_ctx();
        ctx.current_date = Some("Tuesday, August 11, 2026 (2026-08-11 22:29 +00:00)".to_string());
        let prompt = build_system_prompt(&ctx);
        assert!(!prompt.contains("## Current Date"));
        assert!(prompt.contains(
            "<current_date>\nToday is Tuesday, August 11, 2026 (2026-08-11 22:29 +00:00).\n</current_date>"
        ));
    }

    #[test]
    fn test_runtime_data_sections_are_not_markdown_headings() {
        let prompt = build_system_prompt(&full_ctx());
        for heading in [
            "## Current Date",
            "## Live Context",
            "## Your Tools",
            "## Memory",
            "## Skills",
            "## Connected Tool Servers",
            "## Workspace",
            "## Workspace Context",
            "## Identity",
            "## Persona",
            "## User Context",
            "## Long-Term Memory",
            "## User Profile",
            "## Channel",
            "## Sender",
            "## Peer Agents",
        ] {
            assert!(
                !prompt.contains(heading),
                "runtime data still rendered as a markdown heading: {heading}"
            );
        }
        assert!(
            !prompt.contains("### "),
            "workspace context files still use markdown subheadings"
        );
        for tag in [
            "current_date",
            "live_context",
            "your_tools",
            "memory",
            "skills",
            "connected_tool_servers_mcp",
            "workspace",
            "workspace_context",
            "identity",
            "persona",
            "user_context",
            "long_term_memory",
            "user_profile",
            "channel",
            "sender",
            "peer_agents",
        ] {
            assert!(prompt.contains(&format!("<{tag}>")), "missing <{tag}>");
            assert!(prompt.contains(&format!("</{tag}>")), "missing </{tag}>");
        }
    }

    /// The reformatting must not drop anything: the same information has to
    /// reach the model, only the wrapper changes. Without this, a patch that
    /// simply deleted the sections would pass the test above.
    #[test]
    fn test_reformatted_sections_keep_their_data() {
        let prompt = build_system_prompt(&full_ctx());
        for needle in [
            "Today is Tuesday, August 11, 2026 (2026-08-11 22:29 +00:00).",
            "web_search (search the web for information)",
            "[pref] likes dark mode",
            "- web-search: Search the web",
            "Prefer official docs.",
            "- github: 5 tools (search, create_issue)",
            "Workspace: /home/user/project",
            "Amber avatar, calm tone.",
            "Speak like a laconic engineer.",
            "Runs a florist shop.",
            "Prefers metric units.",
            "don't know the user's name yet",
            "You are responding via telegram. Keep messages under 4096 chars.",
            "Message from: Katya (79990001122)",
            "**archivist** (running) — model: glm-5.2",
            "- Project: openfang (Rust)",
            "BTCUSD: 67000",
        ] {
            assert!(
                prompt.contains(needle),
                "data lost from the prompt: {needle}"
            );
        }
    }

    /// Behavioral instruction prose deliberately keeps its markdown headings —
    /// heading structure carries directive weight. Changing that set should be
    /// a conscious act, so pin it.
    #[test]
    fn cap_file_blocks_never_splits_a_file_block() {
        // Five blocks of ~120 chars each against a 300-char budget: a character cap lands
        // inside block three and leaves its opening tag dangling. This is the shape that
        // was reaching 11 of 14 agents on the box.
        let body = (1..=5)
            .map(|i| format!("<file name=\"F{i}.md\">\n{}\n</file>", "x".repeat(100)))
            .collect::<Vec<_>>()
            .join("\n");

        let naive = cap_str(&body, 300);
        assert_ne!(
            naive.matches("<file ").count(),
            naive.matches("</file>").count(),
            "precondition: the plain character cap is what leaves tags unbalanced"
        );

        let capped = cap_file_blocks(&body, 300);
        assert_eq!(
            capped.matches("<file ").count(),
            capped.matches("</file>").count(),
            "every kept block must be closed: {capped}"
        );
        assert!(capped.chars().count() <= 300, "budget respected");
        assert!(
            capped.contains("<omitted blocks="),
            "what was dropped is stated, not silently missing: {capped}"
        );
        // A block that fits is kept verbatim, not trimmed.
        assert!(capped.contains("<file name=\"F1.md\">"));

        // Under budget the body is untouched.
        let small = "- Project: openfang (Rust)\n<file name=\"A.md\">\nhi\n</file>";
        assert_eq!(cap_file_blocks(small, 1000), small);
    }

    #[test]
    fn test_only_instruction_prose_keeps_markdown_headings() {
        let prompt = build_system_prompt(&full_ctx());
        let headings: Vec<&str> = prompt.lines().filter(|l| l.starts_with("## ")).collect();
        assert_eq!(
            headings,
            vec![
                "## Tool Call Behavior",
                "## Heartbeat Checklist",
                "## Safety",
                "## Operational Guidelines",
                "## First-Run Protocol",
            ],
            "markdown-heading sections changed; runtime data must use <tags> (see data_section)"
        );
    }

    #[test]
    fn test_full_prompt_has_all_sections() {
        let prompt = build_system_prompt(&basic_ctx());
        assert!(prompt.contains("You are Researcher"));
        assert!(prompt.contains("## Tool Call Behavior"));
        assert!(prompt.contains("<your_tools>"));
        assert!(prompt.contains("<memory>"));
        assert!(prompt.contains("<user_profile>"));
        assert!(prompt.contains("## Safety"));
        assert!(prompt.contains("## Operational Guidelines"));
    }

    #[test]
    fn test_section_ordering() {
        let prompt = build_system_prompt(&basic_ctx());
        let tool_behavior_pos = prompt.find("## Tool Call Behavior").unwrap();
        let tools_pos = prompt.find("<your_tools>").unwrap();
        let memory_pos = prompt.find("<memory>").unwrap();
        let safety_pos = prompt.find("## Safety").unwrap();
        let guidelines_pos = prompt.find("## Operational Guidelines").unwrap();

        assert!(tool_behavior_pos < tools_pos);
        assert!(tools_pos < memory_pos);
        assert!(memory_pos < safety_pos);
        assert!(safety_pos < guidelines_pos);
    }

    #[test]
    fn test_subagent_omits_sections() {
        let mut ctx = basic_ctx();
        ctx.is_subagent = true;
        let prompt = build_system_prompt(&ctx);

        assert!(!prompt.contains("## Tool Call Behavior"));
        assert!(!prompt.contains("<user_profile>"));
        assert!(!prompt.contains("<channel>"));
        assert!(!prompt.contains("## Safety"));
        // Subagents still get tools and guidelines
        assert!(prompt.contains("<your_tools>"));
        assert!(prompt.contains("## Operational Guidelines"));
        assert!(prompt.contains("<memory>"));
    }

    #[test]
    fn test_empty_tools_no_section() {
        let ctx = PromptContext {
            agent_name: "test".to_string(),
            ..Default::default()
        };
        let prompt = build_system_prompt(&ctx);
        assert!(!prompt.contains("<your_tools>"));
    }

    #[test]
    fn test_tool_grouping() {
        let tools = vec![
            "web_search".to_string(),
            "web_fetch".to_string(),
            "file_read".to_string(),
            "browser_navigate".to_string(),
        ];
        let section = build_tools_section(&tools);
        assert!(section.contains("**Browser**"));
        assert!(section.contains("**Files**"));
        assert!(section.contains("**Web**"));
    }

    #[test]
    fn test_tool_categories() {
        assert_eq!(tool_category("file_read"), "Files");
        assert_eq!(tool_category("web_search"), "Web");
        assert_eq!(tool_category("browser_navigate"), "Browser");
        assert_eq!(tool_category("shell_exec"), "Shell");
        assert_eq!(tool_category("memory_store"), "Memory");
        assert_eq!(tool_category("agent_send"), "Agents");
        assert_eq!(tool_category("mcp_github_search"), "MCP");
        assert_eq!(tool_category("unknown_tool"), "Other");
    }

    #[test]
    fn test_tool_hints() {
        assert!(!tool_hint("web_search").is_empty());
        assert!(!tool_hint("file_read").is_empty());
        assert!(!tool_hint("browser_navigate").is_empty());
        assert!(tool_hint("some_unknown_tool").is_empty());
    }

    #[test]
    fn test_memory_section_empty() {
        let section = build_memory_section(&[]);
        assert!(section.starts_with("<memory>\n"));
        assert!(section.ends_with("</memory>"));
        assert!(section.contains("use memory_recall first"));
        assert!(!section.contains("Recalled memories"));
    }

    #[test]
    fn test_memory_section_with_items() {
        let memories = vec![
            ("pref".to_string(), "User likes dark mode".to_string()),
            ("ctx".to_string(), "Working on Rust project".to_string()),
        ];
        let section = build_memory_section(&memories);
        assert!(section.contains("Recalled memories"));
        assert!(section.contains("[pref] User likes dark mode"));
        assert!(section.contains("[ctx] Working on Rust project"));
        assert!(section.contains("Use the recalled memories below"));
        assert!(!section.contains("use memory_recall first"));
    }

    #[test]
    fn test_memory_cap_at_5() {
        let memories: Vec<(String, String)> = (0..10)
            .map(|i| (format!("k{i}"), format!("value {i}")))
            .collect();
        let section = build_memory_section(&memories);
        assert!(section.contains("[k0]"));
        assert!(section.contains("[k4]"));
        assert!(!section.contains("[k5]"));
    }

    #[test]
    fn test_memory_content_capped() {
        let long_content = "x".repeat(1000);
        let memories = vec![("k".to_string(), long_content)];
        let section = build_memory_section(&memories);
        // Should be capped at 500 + "..."
        assert!(section.contains("..."));
        assert!(section.len() < 1200);
    }

    #[test]
    fn test_skills_section_omitted_when_empty() {
        let ctx = basic_ctx();
        let prompt = build_system_prompt(&ctx);
        assert!(!prompt.contains("<skills>"));
    }

    #[test]
    fn test_skills_section_present() {
        let mut ctx = basic_ctx();
        ctx.skill_summary = "- web-search: Search the web\n- git-expert: Git commands".to_string();
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("<skills>"));
        assert!(prompt.contains("web-search"));
    }

    #[test]
    fn test_mcp_section_omitted_when_empty() {
        let ctx = basic_ctx();
        let prompt = build_system_prompt(&ctx);
        assert!(!prompt.contains("<connected_tool_servers_mcp>"));
    }

    #[test]
    fn test_mcp_section_present() {
        let mut ctx = basic_ctx();
        ctx.mcp_summary = "- github: 5 tools (search, create_issue, ...)".to_string();
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("<connected_tool_servers_mcp>"));
        assert!(prompt.contains("github"));
    }

    #[test]
    fn test_persona_section_with_soul() {
        let mut ctx = basic_ctx();
        ctx.soul_md = Some("You are a pirate. Arr!".to_string());
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("<persona>"));
        assert!(prompt.contains("pirate"));
    }

    #[test]
    fn test_persona_soul_capped_at_1000() {
        let long_soul = "x".repeat(2000);
        let section = build_persona_section(None, Some(&long_soul), None, None, None);
        assert!(section.contains("..."));
        // The raw soul content in the section should be at most 1003 chars (1000 + "...")
        assert!(section.len() < 1200);
    }

    #[test]
    fn test_channel_telegram() {
        let section = build_channel_section("telegram");
        assert!(section.contains("4096"));
        assert!(section.contains("Telegram"));
    }

    #[test]
    fn test_channel_discord() {
        let section = build_channel_section("discord");
        assert!(section.contains("2000"));
        assert!(section.contains("Discord"));
    }

    #[test]
    fn test_channel_irc() {
        let section = build_channel_section("irc");
        assert!(section.contains("512"));
        assert!(section.contains("plain text"));
    }

    #[test]
    fn test_channel_unknown_gets_default() {
        let section = build_channel_section("smoke_signal");
        assert!(section.contains("4096"));
        assert!(section.contains("smoke_signal"));
    }

    #[test]
    fn test_user_name_known() {
        let mut ctx = basic_ctx();
        ctx.user_name = Some("Alice".to_string());
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("Alice"));
        assert!(!prompt.contains("don't know the user's name"));
    }

    #[test]
    fn test_user_name_unknown() {
        let ctx = basic_ctx();
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("don't know the user's name"));
    }

    #[test]
    fn test_canonical_context_not_in_system_prompt() {
        let mut ctx = basic_ctx();
        ctx.canonical_context =
            Some("User was discussing Rust async patterns last time.".to_string());
        let prompt = build_system_prompt(&ctx);
        // Canonical context should NOT be in system prompt (moved to user message)
        assert!(!prompt.contains("## Previous Conversation Context"));
        assert!(!prompt.contains("Rust async patterns"));
        // But should be available via build_canonical_context_message
        let msg = build_canonical_context_message(&ctx);
        assert!(msg.is_some());
        assert!(msg.unwrap().contains("Rust async patterns"));
    }

    #[test]
    fn test_canonical_context_omitted_for_subagent() {
        let mut ctx = basic_ctx();
        ctx.is_subagent = true;
        ctx.canonical_context = Some("Previous context here.".to_string());
        let prompt = build_system_prompt(&ctx);
        assert!(!prompt.contains("Previous Conversation Context"));
        // Should also be None from build_canonical_context_message
        assert!(build_canonical_context_message(&ctx).is_none());
    }

    #[test]
    fn test_empty_base_prompt_generates_default_identity() {
        let ctx = PromptContext {
            agent_name: "helper".to_string(),
            agent_description: "A helpful agent".to_string(),
            ..Default::default()
        };
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("You are helper"));
        assert!(prompt.contains("A helpful agent"));
    }

    #[test]
    fn test_context_md_section_included() {
        let mut ctx = basic_ctx();
        ctx.context_md = Some("BTCUSD: 67000\nETHUSD: 3400".to_string());
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("<live_context>"));
        assert!(prompt.contains("BTCUSD: 67000"));
        assert!(prompt.contains("ETHUSD: 3400"));
    }

    #[test]
    fn test_context_md_section_omitted_when_empty_or_none() {
        let mut ctx = basic_ctx();
        ctx.context_md = None;
        let prompt = build_system_prompt(&ctx);
        assert!(!prompt.contains("<live_context>"));

        ctx.context_md = Some("   \n\n   ".to_string());
        let prompt = build_system_prompt(&ctx);
        assert!(!prompt.contains("<live_context>"));
    }

    #[test]
    fn test_workspace_in_persona() {
        let mut ctx = basic_ctx();
        ctx.workspace_path = Some("/home/user/project".to_string());
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("<workspace>"));
        assert!(prompt.contains("/home/user/project"));
    }

    #[test]
    fn test_cap_str_short() {
        assert_eq!(cap_str("hello", 10), "hello");
    }

    #[test]
    fn test_cap_str_long() {
        let result = cap_str("hello world", 5);
        assert_eq!(result, "hello...");
    }

    #[test]
    fn test_cap_str_multibyte_utf8() {
        // This was panicking with "byte index is not a char boundary" (#38)
        let chinese = "你好世界这是一个测试字符串";
        let result = cap_str(chinese, 4);
        assert_eq!(result, "你好世界...");
        // Exact boundary
        assert_eq!(cap_str(chinese, 100), chinese);
    }

    #[test]
    fn test_cap_str_emoji() {
        let emoji = "👋🌍🚀✨💯";
        let result = cap_str(emoji, 3);
        assert_eq!(result, "👋🌍🚀...");
    }

    #[test]
    fn test_capitalize() {
        assert_eq!(capitalize("files"), "Files");
        assert_eq!(capitalize(""), "");
        assert_eq!(capitalize("MCP"), "MCP");
    }
}
