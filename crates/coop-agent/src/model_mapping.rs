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
        let (target_model, model_info_name) = target_model(spec.kind, model);
        let endpoint = endpoint_for_spec(spec);

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
        ProviderKind::Gemini => {
            Endpoint::from_static("https://generativelanguage.googleapis.com/v1beta/")
        }
        ProviderKind::OpenAi | ProviderKind::OpenAiCompatible => {
            Endpoint::from_static("https://api.openai.com/v1/")
        }
        ProviderKind::Ollama => Endpoint::from_static("http://localhost:11434/"),
    }
}

fn target_model(kind: ProviderKind, model: &str) -> (ModelIden, String) {
    match kind {
        ProviderKind::Anthropic => {
            let model_name = strip_prefix(model, "anthropic/");
            (
                ModelIden::new(AdapterKind::Anthropic, model_name.clone()),
                model_name,
            )
        }
        ProviderKind::Gemini => {
            let model_name = strip_prefix(model, "gemini/");
            (
                ModelIden::new(AdapterKind::Gemini, model_name.clone()),
                model_name,
            )
        }
        ProviderKind::OpenAi => {
            let (adapter_kind, model_name) = openai_like_model(model);
            let info_name = model_name.clone();
            (ModelIden::new(adapter_kind, model_name), info_name)
        }
        ProviderKind::OpenAiCompatible => {
            let (adapter_kind, model_name, info_name) = openai_compatible_model(model);
            (ModelIden::new(adapter_kind, model_name), info_name)
        }
        ProviderKind::Ollama => {
            let model_name = strip_ollama_prefix(model);
            (
                ModelIden::new(AdapterKind::Ollama, model_name.clone()),
                model_name,
            )
        }
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

fn openai_compatible_model(model: &str) -> (AdapterKind, String, String) {
    let normalized = strip_prefix(model, "openai-compatible/");
    if let Some((namespace, name)) = normalized.split_once("::") {
        return match namespace {
            "openai_resp" => (AdapterKind::OpenAIResp, name.to_owned(), name.to_owned()),
            "openai" => (AdapterKind::OpenAI, name.to_owned(), name.to_owned()),
            _ => {
                let (adapter_kind, model_name) = infer_openai_adapter(&normalized);
                (adapter_kind, model_name.clone(), model_name)
            }
        };
    }
    let (adapter_kind, model_name) = infer_openai_adapter(&normalized);
    (adapter_kind, model_name.clone(), model_name)
}

fn infer_openai_adapter(model: &str) -> (AdapterKind, String) {
    let inference_key = model
        .rsplit_once("::")
        .map_or(model, |(_, tail)| tail)
        .rsplit('/')
        .next()
        .unwrap_or(model);

    let uses_responses = (inference_key.starts_with("gpt")
        && (inference_key.contains("codex") || inference_key.contains("pro")))
        || inference_key.starts_with("o1")
        || inference_key.starts_with("o3")
        || inference_key.starts_with("o4")
        || inference_key.starts_with("chatgpt")
        || inference_key.starts_with("codex");

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
    fn gemini_prefix_is_stripped() {
        let spec = ProviderSpec::new(ProviderKind::Gemini, "gemini/gemini-2.5-flash");
        let resolved = ResolvedModel::from_spec(&spec, &spec.model, AuthData::None);
        assert_eq!(resolved.model_info_name, "gemini-2.5-flash");
        assert_eq!(resolved.target_model.adapter_kind, AdapterKind::Gemini);
    }

    #[test]
    fn openai_compatible_defaults_to_openai_adapter_for_custom_names() {
        let spec = ProviderSpec::new(
            ProviderKind::OpenAiCompatible,
            "meta-llama/Llama-3.3-70B-Instruct",
        );
        let resolved = ResolvedModel::from_spec(&spec, &spec.model, AuthData::None);
        assert_eq!(resolved.target_model.adapter_kind, AdapterKind::OpenAI);
        assert_eq!(
            resolved.model_info_name,
            "meta-llama/Llama-3.3-70B-Instruct"
        );
    }

    #[test]
    fn openai_compatible_preserves_openai_namespace_in_model_name() {
        let spec = ProviderSpec::new(
            ProviderKind::OpenAiCompatible,
            "openai/gemma-4-31B-it-UD-Q8_K_XL.gguf",
        );
        let resolved = ResolvedModel::from_spec(&spec, &spec.model, AuthData::None);
        assert_eq!(resolved.target_model.adapter_kind, AdapterKind::OpenAI);
        assert_eq!(
            resolved.model_info_name,
            "openai/gemma-4-31B-it-UD-Q8_K_XL.gguf"
        );
    }

    #[test]
    fn openai_compatible_infers_responses_adapter_from_namespaced_openai_model() {
        let spec = ProviderSpec::new(ProviderKind::OpenAiCompatible, "openai/gpt-5-codex");
        let resolved = ResolvedModel::from_spec(&spec, &spec.model, AuthData::None);
        assert_eq!(resolved.target_model.adapter_kind, AdapterKind::OpenAIResp);
        assert_eq!(resolved.model_info_name, "openai/gpt-5-codex");
    }

    #[test]
    fn openai_compatible_respects_explicit_responses_namespace() {
        let spec = ProviderSpec::new(
            ProviderKind::OpenAiCompatible,
            "openai_resp::openai/gpt-5-mini",
        );
        let resolved = ResolvedModel::from_spec(&spec, &spec.model, AuthData::None);
        assert_eq!(resolved.target_model.adapter_kind, AdapterKind::OpenAIResp);
        assert_eq!(resolved.model_info_name, "openai/gpt-5-mini");
    }

    #[test]
    fn ollama_prefix_is_stripped() {
        let spec = ProviderSpec::new(ProviderKind::Ollama, "ollama/llama3.2");
        let resolved = ResolvedModel::from_spec(&spec, &spec.model, AuthData::None);
        assert_eq!(resolved.target_model.adapter_kind, AdapterKind::Ollama);
        assert_eq!(resolved.model_info_name, "llama3.2");
    }
}
