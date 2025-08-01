use extism_pdk::*;

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[host_fn]
extern "ExtismHost" {
    fn generate_external(prompt: String) -> Json<Result<String, String>>;
    fn chat_external(messages: Json<Vec<ChatMessage>>) -> Json<Result<String, String>>;
}

/// Calls the configured generation model with a single prompt.
fn generate(prompt: &str) -> Result<String, String> {
    unsafe { generate_external(prompt.to_string()).unwrap().0 }
}

/// Sends a list of messages to the chat model and gets a response.
fn chat(messages: Vec<ChatMessage>) -> Result<String, String> {
    unsafe { chat_external(Json(messages)).unwrap().0 }
}

fn call(output: &str, target: &str) -> Result<f32, String> {
    if output.trim() == target.trim() {
        Ok(100.0)
    } else {
        Ok(0.0)
    }
}
        
#[plugin_fn]
pub fn call_external(Json((output, target)): Json<(String, String)>) -> FnResult<Json<Result<f32, String>>> {
    Ok(Json(call(output.as_str(), target.as_str())))
}