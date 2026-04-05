use genai::ModelIden;
use genai::adapter::AdapterKind;
use genai::resolver::{AuthData, Endpoint};

use crate::provider_spec::{ProviderKind, ProviderSpec};

#[derive(Debug, Clone)]
pub(crate) struct ResolvedModel {
    pub model_info_name: String,
    pub target_model: ModelIden,
    pub endpoint: Endpoint,
}

impl ResolvedModel {
    pub(crate) fn from_spec(spec: &ProviderSpec, model: &str, _auth: AuthData) -> Self {
        let target_model = target_model(spec.kind, model);
        let endpoint = endpoint_for_spec(spec);
        let model_info_name = target_model.model_name.to_string();

        Self {
            model_info_name,
            target_model,
            endpoint,
        }
    }

    pub(crate) fn to_service_target(&self, auth: AuthData) -> genai::ServiceTarget {
        genai::ServiceTarget {
            endpoint: self.endpoint.clone(),
            auth,
            model: self.target_model.clone(),
        }
    }
}

fn endpoint_for_spec(spec: &ProviderSpec) -> Endpoint {
    if let Some(base_url) = spec.normalized_base_url() {
        return Endpoint::from_owned(base_url);
    }

    match spec.kind {
        ProviderKind::Anthropic => Endpoint::from_static("https://api.anthropic.com/"),
        ProviderKind::OpenAi | ProviderKind::OpenAiCompatible => {
            Endpoint::from_static("https://api.openai.com/v1/")
        }
        ProviderKind::Ollama => Endpoint::from_static("http://localhost:11434/"),
    }
}

fn target_model(kind: ProviderKind, model: &str) -> ModelIden {
    match kind {
        ProviderKind::Anthropic => {
            ModelIden::new(AdapterKind::Anthropic, strip_prefix(model, "anthropic/"))
        }
        ProviderKind::OpenAi => {
            let (adapter_kind, model_name) = openai_like_model(model);
            ModelIden::new(adapter_kind, model_name)
        }
        ProviderKind::OpenAiCompatible => {
            let (adapter_kind, model_name) = openai_compatible_model(model);
            ModelIden::new(adapter_kind, model_name)
        }
        ProviderKind::Ollama => ModelIden::new(AdapterKind::Ollama, strip_ollama_prefix(model)),
    }
}

fn openai_like_model(model: &str) -> (AdapterKind, String) {
    let normalized = strip_prefix(model, "openai/");
    if let Some((namespace, name)) = normalized.split_once("::") {
        return match namespace {
            "openai_resp" => (AdapterKind::OpenAIResp, name.to_owned()),
            "openai" => (AdapterKind::OpenAI, name.to_owned()),
            _ => infer_openai_adapter(&normalized),
        };
    }
    infer_openai_adapter(&normalized)
}

fn openai_compatible_model(model: &str) -> (AdapterKind, String) {
    let normalized = strip_prefix(model, "openai-compatible/");
    let normalized = strip_prefix(&normalized, "openai/");
    if let Some((namespace, name)) = normalized.split_once("::") {
        return match namespace {
            "openai_resp" => (AdapterKind::OpenAIResp, name.to_owned()),
            _ => (AdapterKind::OpenAI, name.to_owned()),
        };
    }
    infer_openai_adapter(&normalized)
}

fn infer_openai_adapter(model: &str) -> (AdapterKind, String) {
    let uses_responses = (model.starts_with("gpt")
        && (model.contains("codex") || model.contains("pro")))
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
        || model.starts_with("chatgpt")
        || model.starts_with("codex");

    let adapter_kind = if uses_responses {
        AdapterKind::OpenAIResp
    } else {
        AdapterKind::OpenAI
    };
    (adapter_kind, model.to_owned())
}

fn strip_ollama_prefix(model: &str) -> String {
    let stripped = strip_prefix(model, "ollama/");
    stripped
        .strip_prefix("ollama::")
        .unwrap_or(&stripped)
        .to_owned()
}

fn strip_prefix(value: &str, prefix: &str) -> String {
    value.strip_prefix(prefix).unwrap_or(value).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_prefix_is_stripped() {
        let spec = ProviderSpec::new(
            ProviderKind::Anthropic,
            "anthropic/claude-sonnet-4-20250514",
        );
        let resolved = ResolvedModel::from_spec(&spec, &spec.model, AuthData::None);
        assert_eq!(resolved.model_info_name, "claude-sonnet-4-20250514");
        assert_eq!(resolved.target_model.adapter_kind, AdapterKind::Anthropic);
    }

    #[test]
    fn openai_codex_uses_responses_adapter() {
        let spec = ProviderSpec::new(ProviderKind::OpenAi, "gpt-5-codex");
        let resolved = ResolvedModel::from_spec(&spec, &spec.model, AuthData::None);
        assert_eq!(resolved.target_model.adapter_kind, AdapterKind::OpenAIResp);
    }

    #[test]
    fn openai_compatible_defaults_to_openai_adapter_for_custom_names() {
        let spec = ProviderSpec::new(
            ProviderKind::OpenAiCompatible,
            "meta-llama/Llama-3.3-70B-Instruct",
        );
        let resolved = ResolvedModel::from_spec(&spec, &spec.model, AuthData::None);
        assert_eq!(resolved.target_model.adapter_kind, AdapterKind::OpenAI);
    }

    #[test]
    fn openai_compatible_strips_openai_prefix_alias() {
        let spec = ProviderSpec::new(
            ProviderKind::OpenAiCompatible,
            "openai/gemma-4-31B-it-UD-Q8_K_XL.gguf",
        );
        let resolved = ResolvedModel::from_spec(&spec, &spec.model, AuthData::None);
        assert_eq!(resolved.target_model.adapter_kind, AdapterKind::OpenAI);
        assert_eq!(resolved.model_info_name, "gemma-4-31B-it-UD-Q8_K_XL.gguf");
    }

    #[test]
    fn openai_compatible_respects_explicit_responses_namespace() {
        let spec = ProviderSpec::new(ProviderKind::OpenAiCompatible, "openai_resp::gpt-5-mini");
        let resolved = ResolvedModel::from_spec(&spec, &spec.model, AuthData::None);
        assert_eq!(resolved.target_model.adapter_kind, AdapterKind::OpenAIResp);
        assert_eq!(resolved.model_info_name, "gpt-5-mini");
    }

    #[test]
    fn ollama_prefix_is_stripped() {
        let spec = ProviderSpec::new(ProviderKind::Ollama, "ollama/llama3.2");
        let resolved = ResolvedModel::from_spec(&spec, &spec.model, AuthData::None);
        assert_eq!(resolved.target_model.adapter_kind, AdapterKind::Ollama);
        assert_eq!(resolved.model_info_name, "llama3.2");
    }
}
