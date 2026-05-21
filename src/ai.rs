/// AI 调用模块 — 在独立线程上运行 tokio，通过 futures oneshot channel 将结果
/// 返回给 GPUI 的 smol executor。
///
/// 使用方式：
/// ```
/// let rx = ai::send_message(prompt, api_key, base_url, model);
/// cx.spawn(async move |cx| {
///     if let Ok(result) = rx.await {
///         // result: anyhow::Result<String>
///     }
/// }).detach();
/// ```

use futures::channel::oneshot;

/// 发送消息给 AI 模型，返回一个可在 GPUI（smol）executor 中 await 的接收端。
/// 实际 HTTP 调用在独立线程的 tokio runtime 中执行，不阻塞 UI 线程。
pub fn send_message(
    prompt: String,
    api_key: String,
    base_url: String,
    model: String,
) -> oneshot::Receiver<anyhow::Result<String>> {
    let (tx, rx) = oneshot::channel();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt for AI");

        let result = rt.block_on(call_ai(prompt, api_key, base_url, model));
        let _ = tx.send(result);
    });

    rx
}

async fn call_ai(
    prompt: String,
    api_key: String,
    base_url: String,
    model: String,
) -> anyhow::Result<String> {
    use rig_core::{
        client::{CompletionClient, ProviderClient},
        completion::Prompt,
        providers::anthropic,
    };

    if api_key.is_empty() {
        anyhow::bail!("未配置 API Key，请在「AI 设置 → 认证」中填写，或设置 ANTHROPIC_API_KEY 环境变量");
    }

    // 去掉末尾斜杠，避免拼出双斜杠 URL
    let base_url = base_url.trim_end_matches('/').to_string();

    let client = if base_url.is_empty() || base_url == "https://api.anthropic.com" {
        anthropic::Client::from_val(api_key)
            .map_err(|e| anyhow::anyhow!("{}", e))?
    } else {
        anthropic::Client::builder()
            .api_key(api_key)
            .base_url(base_url)
            .build()
            .map_err(|e| anyhow::anyhow!("{}", e))?
    };

    let model_name = if model.is_empty() { "claude-opus-4-5".to_string() } else { model };
    let agent = client.agent(&model_name).max_tokens(4096).build();
    let response = agent.prompt(&prompt).await
        .map_err(|e| anyhow::anyhow!("{}", e))?;
    Ok(response)
}
