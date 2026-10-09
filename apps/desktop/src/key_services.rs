//! The services the Keys page offers to pick from, with their logos.

use std::sync::OnceLock;

use iced::widget::image;

use crate::UsageProvider;

/// Where a service's logo comes from: a provider the app already shows, or
/// its own file in `assets/services`.
#[derive(Clone, Copy)]
enum Logo {
    Provider(UsageProvider),
    File(&'static [u8]),
    /// A white mark, drawn black on a light theme.
    OneColor(&'static [u8]),
}

pub(super) struct Service {
    pub name: &'static str,
    logo: Logo,
}

macro_rules! logo_file {
    ($name:literal) => {
        Logo::File(include_bytes!(concat!(
            "../assets/services/",
            $name,
            ".png"
        )))
    };
}

macro_rules! one_color_logo {
    ($name:literal) => {
        Logo::OneColor(include_bytes!(concat!(
            "../assets/services/",
            $name,
            ".png"
        )))
    };
}

/// Most used first. `vault::detect_service` names its services the same way.
pub(super) static SERVICES: &[Service] = &[
    Service {
        name: "OpenAI",
        logo: Logo::Provider(UsageProvider::Codex),
    },
    Service {
        name: "Claude",
        logo: Logo::Provider(UsageProvider::Claude),
    },
    Service {
        name: "Google Gemini",
        logo: logo_file!("gemini"),
    },
    Service {
        name: "xAI",
        logo: Logo::Provider(UsageProvider::Xai),
    },
    Service {
        name: "DeepSeek",
        logo: Logo::Provider(UsageProvider::DeepSeek),
    },
    Service {
        name: "Mistral",
        logo: logo_file!("mistral"),
    },
    Service {
        name: "Meta Llama",
        logo: logo_file!("meta"),
    },
    Service {
        name: "OpenRouter",
        logo: Logo::Provider(UsageProvider::OpenRouter),
    },
    Service {
        name: "Groq",
        logo: logo_file!("groq"),
    },
    Service {
        name: "Perplexity",
        logo: logo_file!("perplexity"),
    },
    Service {
        name: "Kimi",
        logo: Logo::Provider(UsageProvider::Kimi),
    },
    Service {
        name: "Z.ai",
        logo: Logo::Provider(UsageProvider::Zai),
    },
    Service {
        name: "MiniMax",
        logo: Logo::Provider(UsageProvider::MiniMax),
    },
    Service {
        name: "Qwen",
        logo: logo_file!("qwen"),
    },
    Service {
        name: "Xiaomi MiMo",
        logo: Logo::Provider(UsageProvider::MiMo),
    },
    Service {
        name: "Cohere",
        logo: logo_file!("cohere"),
    },
    Service {
        name: "Together AI",
        logo: logo_file!("together"),
    },
    Service {
        name: "Fireworks AI",
        logo: logo_file!("fireworks"),
    },
    Service {
        name: "Cerebras",
        logo: logo_file!("cerebras"),
    },
    Service {
        name: "Hugging Face",
        logo: logo_file!("huggingface"),
    },
    Service {
        name: "ElevenLabs",
        logo: logo_file!("elevenlabs"),
    },
    Service {
        name: "NVIDIA",
        logo: logo_file!("nvidia"),
    },
    Service {
        name: "Azure OpenAI",
        logo: logo_file!("azure"),
    },
    Service {
        name: "Amazon Bedrock",
        logo: logo_file!("bedrock"),
    },
    Service {
        name: "Vertex AI",
        logo: logo_file!("vertexai"),
    },
    Service {
        name: "GitHub",
        logo: one_color_logo!("github"),
    },
    Service {
        name: "Replicate",
        logo: one_color_logo!("replicate"),
    },
    Service {
        name: "Ollama",
        logo: one_color_logo!("ollama"),
    },
    Service {
        name: "fal",
        logo: logo_file!("fal"),
    },
    Service {
        name: "DeepInfra",
        logo: logo_file!("deepinfra"),
    },
    Service {
        name: "SambaNova",
        logo: logo_file!("sambanova"),
    },
    Service {
        name: "Novita AI",
        logo: logo_file!("novita"),
    },
    Service {
        name: "Hyperbolic",
        logo: logo_file!("hyperbolic"),
    },
    Service {
        name: "Nebius",
        logo: one_color_logo!("nebius"),
    },
    Service {
        name: "SiliconFlow",
        logo: logo_file!("siliconcloud"),
    },
    Service {
        name: "Cloudflare Workers AI",
        logo: logo_file!("workersai"),
    },
    Service {
        name: "Vercel",
        logo: one_color_logo!("vercel"),
    },
    Service {
        name: "Poe",
        logo: logo_file!("poe"),
    },
    Service {
        name: "Venice",
        logo: logo_file!("venice"),
    },
    Service {
        name: "AI21 Labs",
        logo: one_color_logo!("ai21"),
    },
    Service {
        name: "Reka",
        logo: one_color_logo!("reka"),
    },
    Service {
        name: "Upstage",
        logo: logo_file!("upstage"),
    },
    Service {
        name: "StepFun",
        logo: logo_file!("stepfun"),
    },
    Service {
        name: "Doubao",
        logo: logo_file!("doubao"),
    },
    Service {
        name: "Baidu Qianfan",
        logo: logo_file!("baiducloud"),
    },
    Service {
        name: "Tencent Hunyuan",
        logo: logo_file!("hunyuan"),
    },
    Service {
        name: "Stability AI",
        logo: logo_file!("stability"),
    },
    Service {
        name: "Black Forest Labs",
        logo: one_color_logo!("bfl"),
    },
    Service {
        name: "Runway",
        logo: one_color_logo!("runway"),
    },
    Service {
        name: "Luma AI",
        logo: logo_file!("luma"),
    },
    Service {
        name: "Kling AI",
        logo: logo_file!("kling"),
    },
    Service {
        name: "Ideogram",
        logo: one_color_logo!("ideogram"),
    },
    Service {
        name: "AssemblyAI",
        logo: logo_file!("assemblyai"),
    },
    Service {
        name: "Voyage AI",
        logo: one_color_logo!("voyage"),
    },
    Service {
        name: "Jina AI",
        logo: one_color_logo!("jina"),
    },
    Service {
        name: "Tavily",
        logo: logo_file!("tavily"),
    },
    Service {
        name: "Exa",
        logo: one_color_logo!("exa"),
    },
    Service {
        name: "Firecrawl",
        logo: logo_file!("firecrawl"),
    },
    Service {
        name: "Weights & Biases",
        logo: logo_file!("wandb"),
    },
    Service {
        name: "OpenCode",
        logo: Logo::Provider(UsageProvider::OpenCodeGo),
    },
];

/// Names keys were saved under before, or that people write, for a service.
const ALIASES: &[(&str, &str)] = &[
    ("anthropic", "Claude"),
    ("gemini", "Google Gemini"),
    ("google", "Google Gemini"),
    ("chatgpt", "OpenAI"),
    ("moonshot", "Kimi"),
    ("zai", "Z.ai"),
    ("mimo", "Xiaomi MiMo"),
    ("together", "Together AI"),
    ("fireworks", "Fireworks AI"),
    ("huggingface", "Hugging Face"),
    ("mistral ai", "Mistral"),
];

/// The listed service `name` stands for, if any, ignoring case.
pub(super) fn find(name: &str) -> Option<&'static Service> {
    let name = name.trim();
    let listed = ALIASES
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
        .map_or(name, |(_, listed)| listed);
    SERVICES
        .iter()
        .find(|service| service.name.eq_ignore_ascii_case(listed))
}

/// The place in [`SERVICES`] of the service `name` stands for.
pub(super) fn index_of(name: &str) -> Option<usize> {
    let service = find(name)?;
    SERVICES
        .iter()
        .position(|listed| std::ptr::eq(listed, service))
}

impl Service {
    /// The logo drawn for the active theme.
    pub fn logo(&self, light_theme: bool) -> image::Handle {
        if let Logo::Provider(provider) = self.logo {
            return crate::provider_logo_handle(provider, light_theme);
        }
        // Each file logo decoded once, as drawn on a dark and a light theme.
        static LOGOS: OnceLock<Vec<Option<[image::Handle; 2]>>> = OnceLock::new();
        let logos = LOGOS.get_or_init(|| {
            SERVICES
                .iter()
                .map(|service| match service.logo {
                    Logo::File(bytes) => {
                        let logo = crate::graphics::decode_provider_logo(bytes, false);
                        Some([logo.clone(), logo])
                    }
                    Logo::OneColor(bytes) => Some([
                        crate::graphics::decode_provider_logo(bytes, false),
                        crate::graphics::decode_provider_logo(bytes, true),
                    ]),
                    Logo::Provider(_) => None,
                })
                .collect()
        });
        let index = SERVICES
            .iter()
            .position(|service| std::ptr::eq(service, self))
            .expect("services come from the list");
        logos[index].as_ref().expect("a file logo was decoded")[usize::from(light_theme)].clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detected_services_are_listed() {
        for key in [
            "sk-ant-x",
            "sk-or-x",
            "sk-proj-x",
            "AIza",
            "xai-x",
            "gsk_x",
            "pplx-x",
            "hf_x",
            "github_pat_x",
            "r8_x",
            "fw_x",
            "nvapi-x",
            "tvly-x",
            "jina_x",
        ] {
            let service = usage_monitor_core::vault::detect_service(key).unwrap();
            assert!(find(service).is_some(), "{service} is not listed");
        }
    }

    #[test]
    fn services_are_found_by_name_or_alias() {
        assert_eq!(find(" openai ").map(|s| s.name), Some("OpenAI"));
        assert_eq!(find("Anthropic").map(|s| s.name), Some("Claude"));
        assert_eq!(find("gemini").map(|s| s.name), Some("Google Gemini"));
        assert!(find("My own service").is_none());
    }

    #[test]
    fn every_logo_decodes() {
        for service in SERVICES {
            service.logo(false);
            service.logo(true);
        }
    }
}
