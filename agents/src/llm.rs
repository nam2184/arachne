use async_trait::async_trait;

use crate::domain::{LlmMessage, LlmResponse};

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, model: &str, messages: &[LlmMessage]) -> Result<LlmResponse, String>;

    fn complete_sync(&self, model: &str, messages: &[LlmMessage]) -> Result<String, String> {
        let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
        runtime.block_on(self.complete(model, messages)).map(|response| response.content)
    }

    fn provider_name(&self) -> &str;
    fn supported_models(&self) -> Vec<String>;
}
