//! Transcript helpers the agent loop needs.
//!
//! Ground truth: `packages/ai/src/utils/transcript.ts` (`getCurrentTools`,
//! `getCurrentSystemMessage`, `getCurrentSystemPrompt`, `toToolDeclaration`,
//! `declarationsEqual`, `getToolStateChanges`, `createInitialSystemMessage`)
//! plus `declareToolChanges` in `packages/agent/src/agent-loop.ts`.
//!
//! The system prompt and the tool declarations live in the transcript's system
//! messages, never in separate fields — that is the invariant the loop depends
//! on, and this module is where it is enforced.

use crate::agent_message::AgentMessage;
use crate::types::{AgentContext, ToolExecutor, ToolSpec};
use pi_ai::types::{StringOrTextBlocks, SystemMessage, Tool, ToolReference, UserContent};

/// `toToolDeclaration`: strip everything but what the model sees. Serialized
/// declaration equality is how pi compares two tool sets.
///
/// `constrainedSampling` is part of that interface — pi carries it through so
/// a change from grammar-constrained to plain flips the declaration — and the
/// key is absent entirely when the tool declares none
/// (`transcript.ts:123-130`, key order pi's, which the derived `Serialize` on
/// [`Tool`] preserves through its declaration order).
pub fn tool_declaration(spec: &ToolSpec) -> Tool {
    Tool {
        name: spec.name.clone(),
        description: spec.description.clone(),
        parameters: spec.parameters.clone(),
        constrained_sampling: spec.constrained_sampling.clone(),
    }
}

fn declaration_key(tool: &Tool) -> String {
    serde_json::to_string(tool).unwrap_or_default()
}

/// `getCurrentTools`: replay every system message's tool changes by name.
pub fn current_tools(messages: &[AgentMessage]) -> Vec<Tool> {
    let mut order: Vec<String> = Vec::new();
    let mut tools: Vec<(String, Tool)> = Vec::new();
    for message in messages {
        let AgentMessage::System(system) = message else {
            continue;
        };
        for removed in system.tools_removed.iter().flatten() {
            let ToolReference { name } = removed;
            tools.retain(|(existing, _)| existing != name);
            order.retain(|existing| existing != name);
        }
        for added in system.tools_added.iter().flatten() {
            let name = &added.name;
            if let Some(slot) = tools.iter_mut().find(|(existing, _)| existing == name) {
                slot.1 = added.clone();
            } else {
                order.push(name.clone());
                tools.push((name.clone(), added.clone()));
            }
        }
    }
    order
        .iter()
        .filter_map(|name| {
            tools
                .iter()
                .find(|(existing, _)| existing == name)
                .map(|(_, tool)| tool.clone())
        })
        .collect()
}

/// `getToolStateChanges`: a changed definition is a removal plus an addition.
pub fn tool_state_changes(previous: &[Tool], current: &[Tool]) -> (Vec<Tool>, Vec<ToolReference>) {
    let added = current
        .iter()
        .filter(|tool| {
            match previous
                .iter()
                .find(|candidate| candidate.name == tool.name)
            {
                None => true,
                Some(candidate) => declaration_key(candidate) != declaration_key(tool),
            }
        })
        .cloned()
        .collect();
    let removed = previous
        .iter()
        .filter(
            |tool| match current.iter().find(|candidate| candidate.name == tool.name) {
                None => true,
                Some(candidate) => declaration_key(candidate) != declaration_key(tool),
            },
        )
        .map(|tool| ToolReference {
            name: tool.name.clone(),
        })
        .collect();
    (added, removed)
}

/// `createInitialSystemMessage`: the leading system message, if there is a
/// prompt or a tool set.
pub fn create_initial_system_message(
    system_prompt: Option<&str>,
    tools: &[Tool],
) -> Option<SystemMessage> {
    let has_prompt = system_prompt.is_some_and(|prompt| !prompt.is_empty());
    if !has_prompt && tools.is_empty() {
        return None;
    }
    Some(SystemMessage {
        content: StringOrTextBlocks::Text(system_prompt.unwrap_or_default().to_string()),
        sections: None,
        tools_added: if tools.is_empty() {
            None
        } else {
            Some(tools.to_vec())
        },
        tools_removed: None,
        timestamp: 0,
    })
}

/// `declareToolChanges` (`agent-loop.ts`).
///
/// `context.tools` is what the runtime can execute; the transcript's system
/// messages declare what the model may call. Before each request the difference
/// becomes `toolsAdded`/`toolsRemoved` on a system message. A pending system
/// message is treated as *intent*: its tool fields are replaced with the delta
/// between the committed transcript and the executable set, so a replay always
/// yields exactly `context.tools`. Otherwise a new system message is inserted
/// before the first non-system pending message.
pub fn declare_tool_changes(
    context: &AgentContext,
    executor: &dyn ToolExecutor,
    pending_messages: Vec<AgentMessage>,
) -> Vec<AgentMessage> {
    let current: Vec<Tool> = executor.tools().iter().map(tool_declaration).collect();
    let system_index = pending_messages
        .iter()
        .rposition(|message| message.role() == "system");
    let Some(system_index) = system_index else {
        let mut baseline = pending_messages.clone();
        let mut replay = context.messages.clone();
        replay.extend(pending_messages.iter().cloned());
        let (added, removed) = tool_state_changes(&current_tools(&replay), &current);
        if added.is_empty() && removed.is_empty() {
            return pending_messages;
        }
        let update = SystemMessage {
            content: StringOrTextBlocks::Text(String::new()),
            sections: None,
            tools_added: if added.is_empty() { None } else { Some(added) },
            tools_removed: if removed.is_empty() {
                None
            } else {
                Some(removed)
            },
            timestamp: crate::agent_loop::now_millis(),
        };
        let insert_index = pending_messages
            .iter()
            .position(|message| message.role() != "system")
            .unwrap_or(pending_messages.len());
        baseline.insert(insert_index, AgentMessage::System(update));
        return baseline;
    };

    let AgentMessage::System(pending) = pending_messages[system_index].clone() else {
        unreachable!("role() == system implies a system message")
    };
    let baseline: Vec<AgentMessage> = pending_messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            if index == system_index {
                AgentMessage::System(with_tool_changes(&pending, Vec::new(), None))
            } else {
                message.clone()
            }
        })
        .collect();

    let mut replay = context.messages.clone();
    replay.extend(baseline.iter().cloned());
    let (added, removed) = tool_state_changes(&current_tools(&replay), &current);
    let unchanged = added.is_empty() && removed.is_empty();
    let pending_declares_nothing = pending.tools_added.as_ref().is_none_or(Vec::is_empty)
        && pending.tools_removed.as_ref().is_none_or(Vec::is_empty);

    if unchanged && pending_declares_nothing {
        return pending_messages;
    }

    baseline
        .into_iter()
        .enumerate()
        .map(|(index, message)| {
            if index == system_index {
                AgentMessage::System(with_tool_changes(
                    &pending,
                    added.clone(),
                    if removed.is_empty() {
                        None
                    } else {
                        Some(removed.clone())
                    },
                ))
            } else {
                message
            }
        })
        .collect()
}

/// `withToolChanges`: empty lists omit the field entirely.
fn with_tool_changes(
    message: &SystemMessage,
    tools_added: Vec<Tool>,
    tools_removed: Option<Vec<ToolReference>>,
) -> SystemMessage {
    SystemMessage {
        content: message.content.clone(),
        sections: message.sections.clone(),
        tools_added: if tools_added.is_empty() {
            None
        } else {
            Some(tools_added)
        },
        tools_removed,
        timestamp: message.timestamp,
    }
}

/// Build a user message, the shape `Agent.prompt(string)` produces.
pub fn user_message(text: &str, timestamp: i64) -> AgentMessage {
    AgentMessage::User(pi_ai::types::UserMessage {
        content: UserContent::Text(text.to_string()),
        timestamp,
    })
}

/// pi's `prompt(text, { images })` (`agent-session.ts:2140-2160`): the same
/// user message with an image block after the text, which is how an `@file`
/// attachment or a pasted image reaches the provider.
pub fn user_message_with_images(
    text: &str,
    images: &[pi_ai::types::ImageContent],
    timestamp: i64,
) -> AgentMessage {
    let mut content = vec![pi_ai::types::InputContent::Text(
        pi_ai::types::TextContent {
            text: text.to_string(),
            text_signature: None,
        },
    )];
    content.extend(
        images
            .iter()
            .cloned()
            .map(pi_ai::types::InputContent::Image),
    );
    AgentMessage::User(pi_ai::types::UserMessage {
        content: UserContent::Blocks(content),
        timestamp,
    })
}

/// `getCurrentSystemPrompt`: the replayed system prompt text.
pub fn current_system_prompt(messages: &[AgentMessage]) -> String {
    messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::System(system) => Some(&system.content),
            _ => None,
        })
        .map(|content| match content {
            StringOrTextBlocks::Text(text) => text.clone(),
            StringOrTextBlocks::Blocks(blocks) => blocks
                .iter()
                .map(|block| block.text.clone())
                .collect::<Vec<_>>()
                .join("\n"),
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `getCurrentSystemMessage`: replay every system message into one.
pub fn current_system_message(messages: &[AgentMessage]) -> Option<SystemMessage> {
    let systems: Vec<&SystemMessage> = messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::System(system) => Some(system),
            _ => None,
        })
        .collect();
    if systems.is_empty() {
        return None;
    }
    let tools = current_tools(messages);
    let mut sections = std::collections::BTreeMap::new();
    for system in &systems {
        for (name, value) in system.sections.iter().flatten() {
            sections.insert(name.clone(), value.clone());
        }
    }
    Some(SystemMessage {
        content: StringOrTextBlocks::Text(current_system_prompt(messages)),
        sections: if sections.is_empty() {
            None
        } else {
            Some(sections)
        },
        tools_added: if tools.is_empty() { None } else { Some(tools) },
        tools_removed: None,
        timestamp: systems[0].timestamp,
    })
}
