//! Synthetic provider values and explicit alpha.30 projection expectations.
use serde_json::{Value, json};

fn inline() -> Value {
    json!({"inlineData":{"mimeType":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aCzcAAAAASUVORK5CYII="}})
}
fn file() -> Value {
    json!({"fileData":{"mimeType":"application/pdf","fileUri":"gs://fixture-only/report.pdf"}})
}
fn schema() -> Value {
    json!({"type":"object","properties":{"value":{"type":"integer"}},"required":["value"]})
}
fn tool_schema() -> Value {
    json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]})
}
fn call() -> Value {
    json!({"functionCall":{"name":"lookup","args":{"query":"example"}}})
}
fn signed_call(id: &str) -> Value {
    json!({
        "functionCall":{"id":id,"name":"lookup","args":{"query":"example"}},
        "thoughtSignature":"c2lnbmF0dXJl"
    })
}
fn result() -> Value {
    json!({"functionResponse":{"id":"earlier-call","name":"lookup","response":{"value":7}}})
}
fn safety() -> Value {
    json!([{"category":"HARM_CATEGORY_HARASSMENT","threshold":"BLOCK_ONLY_HIGH"}])
}
fn tool_config() -> Value {
    // Multi-name restriction has no canonical single-tool equivalent.
    json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["lookup","clock"]}})
}
fn usage_in() -> Value {
    json!({
        "promptTokenCount":11, "candidatesTokenCount":3, "thoughtsTokenCount":2,
        "cachedContentTokenCount":4, "totalTokenCount":900, "toolUsePromptTokenCount":5
    })
}
fn usage_out() -> Value {
    json!({
        "promptTokenCount":11, "candidatesTokenCount":3, "thoughtsTokenCount":2,
        "cachedContentTokenCount":4, "totalTokenCount":16
    })
}
fn grounding() -> Value {
    json!({"groundingChunks":[{"web":{"uri":"https://fixture.invalid/citation","title":"Fixture citation"}}]})
}

pub(super) fn rich_request(stream: bool) -> Value {
    let mut parameters = tool_schema();
    parameters["additionalProperties"] = json!(false);
    json!({
        "model":{"must_be_overwritten":"unroutable"},
        "stream":stream,
        "systemInstruction":{"parts":[{"text":"one"},{"text":"two"}]},
        "contents":[
            {"role":"user","parts":[{"text":"start"},inline(),file()]},
            {"role":"model","parts":[{"text":"thinking","thought":true,"thoughtSignature":"dGhvdWdodA=="},signed_call("earlier-call")]},
            {"role":"function","parts":[result()]}
        ],
        "tools":[
            {"functionDeclarations":[
                {"name":"lookup","description":"find value","parameters":parameters},
                {"name":"clock","description":"find time","parameters":{"type":"object"}}
            ]},
            {"googleSearch":{}},
            {"codeExecution":{}}
        ],
        "toolConfig":tool_config(),
        "safetySettings":safety(),
        "cachedContent":"cachedContents/fixture-owned",
        "generationConfig":{
            "temperature":0.0, "maxOutputTokens":17, "topP":null, "topK":4, "seed":9,
            "stopSequences":["end"], "candidateCount":2,
            "responseMimeType":"application/json","responseSchema":schema(),
            "thinkingConfig":{"thinkingBudget":64,"includeThoughts":true},
            "unknownFixtureConfig":"dropped"
        }
    })
}

pub(super) fn provider_request() -> Value {
    let mut signed = call();
    signed["thoughtSignature"] = json!("c2lnbmF0dXJl");
    json!({
        "systemInstruction":{"parts":[{"text":"one\ntwo"}]},
        "contents":[
            {"role":"user","parts":[{"text":"start"},inline(),file()]},
            {"role":"model","parts":[{"text":"thinking","thought":true,"thoughtSignature":"dGhvdWdodA=="},signed]},
            {"role":"user","parts":[result()]}
        ],
        "tools":[
            {"functionDeclarations":[
                {"name":"lookup","description":"find value","parameters":tool_schema()},
                {"name":"clock","description":"find time","parameters":{"type":"object"}}
            ]},
            {"googleSearch":{}},
            {"codeExecution":{}}
        ],
        "toolConfig":tool_config(),
        "safetySettings":safety(),
        "cachedContent":"cachedContents/fixture-owned",
        "generationConfig":{
            "temperature":0.0, "maxOutputTokens":17, "topK":4, "seed":9,
            "stopSequences":["end"], "candidateCount":2,
            "responseMimeType":"application/json","responseSchema":schema(),
            "thinkingConfig":{"thinkingBudget":64,"includeThoughts":true}
        }
    })
}

pub(super) fn json_reply() -> Value {
    let mut g = grounding();
    g["webSearchQueries"] = json!(["unprojected"]);
    json!({
        "modelVersion":"gemini-fixture-build-7",
        "responseId":"provider-private-id",
        "candidates":[
            {"index":7,
             "content":{"role":"model","parts":[{"text":"answer"},signed_call("provider-call-id"),inline(),file(),
                {"executableCode":{"language":"PYTHON","code":"print(7)"}}]},
             "finishReason":"RECITATION","groundingMetadata":g,
             "safetyRatings":[{"category":"HARM_CATEGORY_HARASSMENT","probability":"LOW"}]},
            {"index":8,"content":{"role":"model","parts":[{"text":"unprojected-second-candidate"}]},"finishReason":"STOP"}
        ],
        "promptFeedback":{"safetyRatings":[]},
        "usageMetadata":usage_in()
    })
}

pub(super) fn public_json() -> Value {
    let mut signed = call();
    signed["thoughtSignature"] = json!("c2lnbmF0dXJl");
    json!({
        "modelVersion":"gemini-fixture-build-7",
        "candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"answer"},signed,inline(),file()]},
            "finishReason":"RECITATION","groundingMetadata":grounding()}],
        "usageMetadata":usage_out()
    })
}

pub(super) fn stream_frames() -> Vec<Value> {
    vec![
        json!({"responseId":"provider-private-id","modelVersion":"gemini-fixture-build-7",
            "candidates":[{"index":7,"content":{"role":"model","parts":[
                {"text":"first"}, {"text":"reason","thought":true,"thoughtSignature":"dGhvdWdodA=="},
                signed_call("provider-call-id"),inline()
            ]},"groundingMetadata":grounding()}]}),
        json!({"responseId":"provider-private-id","modelVersion":"gemini-fixture-build-7",
            "candidates":[{"index":7,"content":{"role":"model","parts":[{"text":"last"}]},
                "finishReason":"RECITATION","groundingMetadata":grounding()}]}),
        json!({"responseId":"provider-private-id","usageMetadata":usage_in()}),
    ]
}

fn part(value: Value) -> Value {
    json!({"candidates":[{"content":{"role":"model","parts":[value]}}]})
}

pub(super) fn public_stream() -> Vec<Value> {
    vec![
        part(json!({"text":"first"})),
        part(json!({"text":"reason","thought":true})),
        part(call()),
        part(inline()),
        json!({"candidates":[{"content":{"role":"model","parts":[]},"groundingMetadata":grounding()}]}),
        part(json!({"text":"last"})),
        json!({"usageMetadata":usage_out()}),
        json!({"candidates":[{"content":{"role":"model","parts":[]},"finishReason":"SAFETY"}]}),
    ]
}
