//! Tool renderer modules, one per TS `renderers/*.ts` file.

pub mod bash;
pub mod edit;
pub mod find;
pub mod grep;
pub mod index;
pub mod ls;
pub mod read;
pub mod write;

pub use index::{
    CallContext, ResultContext, ToolRenderers, create_all_tool_renderers,
    format_tool_call_with_args, invalid_arg_text, render_tool_path, renderers_for,
    with_built_in_renderers,
};
