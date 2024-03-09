mod gemma;
mod llama;
mod mistral;
mod mixtral;
use candle_sampling::logits_processor::Logprobs;
use either::Either;
pub use gemma::{GemmaLoader, GemmaSpecificConfig};
use hf_hub::{
    api::sync::{ApiBuilder, ApiRepo},
    Repo, RepoType,
};
pub use llama::{LlamaLoader, LlamaSpecificConfig};
use minijinja::{context, Environment};
pub use mistral::{MistralLoader, MistralSpecificConfig};
use mistralrs_lora::{LoraConfig, Ordering};
pub use mixtral::{MixtralLoader, MixtralSpecificConfig};
use serde::Deserialize;
use std::{
    cell::RefCell, collections::HashMap, fs, iter::repeat, path::PathBuf, rc::Rc, str::FromStr,
    sync::Mutex,
};
use tokenizers::Tokenizer;

use anyhow::Result;
use candle_core::{DType, Device, Tensor};

use crate::{
    deref_refcell, models::Cache, sequence::Sequence, utils::tokens::get_token,
    xlora_models::XLoraConfig,
};

pub trait ModelPaths {
    fn get_weight_filenames(&self) -> &[PathBuf];
    fn get_config_filename(&self) -> &PathBuf;
    fn get_tokenizer_filename(&self) -> &PathBuf;
    fn get_template_filename(&self) -> &PathBuf;
    fn get_adapter_filenames(&self) -> &Option<Vec<(String, PathBuf)>>;
    fn get_adapter_configs(&self) -> &Option<Vec<(String, LoraConfig)>>;
    fn get_classifier_path(&self) -> &Option<PathBuf>;
    fn get_classifier_config(&self) -> &Option<XLoraConfig>;
    fn get_ordering(&self) -> &Option<Ordering>;
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct AddedTokensDecoder {
    __type: Option<String>,
    content: String,
    lstrip: bool,
    normalized: bool,
    rstrip: bool,
    single_word: bool,
    special: Option<bool>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct ChatTemplate {
    add_bos_token: bool,
    add_eos_token: bool,
    added_tokens_decoder: Option<HashMap<String, AddedTokensDecoder>>,
    additional_special_tokens: Option<Vec<String>>,
    #[serde(with = "either::serde_untagged")]
    bos_token: Either<String, AddedTokensDecoder>,
    chat_template: Option<String>,
    clean_up_tokenization_spaces: bool,
    device_map: Option<String>,
    #[serde(with = "either::serde_untagged")]
    eos_token: Either<String, AddedTokensDecoder>,
    legacy: Option<bool>,
    model_max_length: f64,
    pad_token: Option<String>,
    sp_model_kwargs: HashMap<String, String>,
    spaces_between_special_tokens: bool,
    tokenizer_class: String,
    truncation_size: Option<String>,
    #[serde(with = "either::serde_untagged")]
    unk_token: Either<String, AddedTokensDecoder>,
    use_default_system_prompt: bool,
}

pub enum TokenSource {
    Literal(String),
    EnvVar(String),
    Path(String),
    CacheToken,
}

pub enum ModelKind {
    Normal,
    XLoraNormal,
    XLoraGGUF,
    XLoraGGML,
    QuantizedGGUF,
    QuantizedGGML,
}

/// Encapsulate downloading and setting up the model. The `load_model` method is used to create the pipeline.
pub trait Loader {
    fn download_model(
        &self,
        revision: Option<String>,
        token_source: TokenSource,
    ) -> Result<Box<dyn ModelPaths>>;

    #[allow(clippy::type_complexity)]
    fn _setup_model(
        &self,
        paths: &dyn ModelPaths,
        dtype: Option<DType>,
        device: &Device,
    ) -> Result<Box<Mutex<dyn Pipeline + Send + Sync>>>;

    /// If `revision` is None, then it defaults to `main`.
    /// If `dtype` is None, then it defaults to the model default (usually F32). TODO(EricLBuehler): refine
    #[allow(clippy::type_complexity)]
    fn load_model(
        &self,
        revision: Option<String>,
        token_source: TokenSource,
        dtype: Option<DType>,
        device: &Device,
    ) -> Result<Box<Mutex<dyn Pipeline + Send + Sync>>> {
        let paths = self.download_model(revision, token_source)?;
        self._setup_model(&*paths, dtype, device)
    }
}

pub trait Pipeline: Send + Sync {
    fn forward(&mut self, input_toks: Box<[Rc<RefCell<Sequence>>]>, is_prompt: bool) -> Tensor;
    fn tokenize_prompt(&self, prompt: &str) -> Result<Vec<u32>> {
        let encoding = self
            .tokenizer()
            .encode(prompt, false)
            .map_err(|e| anyhow::Error::msg(e.to_string()))?;
        Ok(encoding.get_ids().to_vec())
    }
    fn device(&self) -> &Device;
    fn num_hidden_layers(&self) -> usize;
    fn cache(&self) -> &Cache;
    fn sample(&mut self, logits: Tensor, seq: Rc<RefCell<Sequence>>) -> Result<Logprobs>;
    fn tokenizer(&self) -> Tokenizer;
    fn eos_tok(&self) -> u32;
    fn name(&self) -> &'static str;
    fn get_max_seq_len(&self) -> usize;
    fn is_xlora(&self) -> bool;
    fn has_no_kv_cache(&self) -> bool;
    fn apply_chat_template(
        &self,
        messages: Vec<HashMap<String, String>>,
        add_generation_prompt: bool,
    ) -> Result<String> {
        let mut env = Environment::new();
        env.add_template(
            "chat_template",
            self.get_chat_template().chat_template.as_ref().unwrap(),
        )?;
        let tmpl = env.get_template("chat_template").unwrap();
        let bos_tok = match self.get_chat_template().bos_token {
            Either::Left(ref lit) => lit,
            Either::Right(ref added) => &added.content,
        };
        let eos_tok = match self.get_chat_template().eos_token {
            Either::Left(ref lit) => lit,
            Either::Right(ref added) => &added.content,
        };
        let unk_tok = match self.get_chat_template().unk_token {
            Either::Left(ref lit) => lit,
            Either::Right(ref added) => &added.content,
        };
        Ok(tmpl.render(context! {
            messages => messages,
            add_generation_prompt => add_generation_prompt,
            bos_token => bos_tok,
            eos_token => eos_tok,
            unk_token => unk_tok,
        })?)
    }
    fn get_chat_template(&self) -> &ChatTemplate;
}

fn get_prompt_input(input_toks: &[Rc<RefCell<Sequence>>], device: &Device) -> (Tensor, Vec<usize>) {
    // NOTE(EricLBuehler): Unwrap reasoning: Get the maximum sequence length.
    let max_len = input_toks
        .iter()
        .map(|seq| deref_refcell!(seq).len())
        .max()
        .unwrap();
    let padding_tok = 0;
    // Pad each sequence by the padding token to the max len.
    let mut seqs_tensors = Vec::new();
    let mut seqlen_offsets = Vec::new();
    for seq in input_toks.iter() {
        let mut ctxt = deref_refcell!(seq).get_toks().to_vec();
        seqlen_offsets.push(0);

        ctxt.extend(repeat(padding_tok).take(max_len - ctxt.len()));

        // NOTE(EricLBuehler): Unwrap reasoning: The dimensions must match.
        seqs_tensors.push(Tensor::new(ctxt, device).unwrap().unsqueeze(0).unwrap());
    }
    // NOTE(EricLBuehler): Unwrap reasoning: Correct dimensions are provided.
    (Tensor::cat(&seqs_tensors, 0).unwrap(), seqlen_offsets)
}

fn get_completion_input(
    input_toks: &[Rc<RefCell<Sequence>>],
    device: &Device,
    no_kv_cache: bool,
) -> (Tensor, Vec<usize>) {
    if no_kv_cache {
        return get_prompt_input(input_toks, device);
    }

    // Pad each sequence by the padding token to the max len.
    let mut seqs_tensors = Vec::new();
    let mut seqlen_offsets = Vec::new();
    for seq in input_toks.iter() {
        let start_pos = deref_refcell!(seq).get_toks().len().saturating_sub(1);
        let ctxt = deref_refcell!(seq).get_toks()[start_pos..].to_vec();
        seqlen_offsets.push(start_pos);

        // NOTE(EricLBuehler): Unwrap reasoning: The dimensions must match.
        seqs_tensors.push(Tensor::new(ctxt, device).unwrap().unsqueeze(0).unwrap());
    }
    // NOTE(EricLBuehler): Unwrap reasoning: Correct dimensions are provided.
    (Tensor::cat(&seqs_tensors, 0).unwrap(), seqlen_offsets)
}

struct XLoraPaths {
    adapter_configs: Option<Vec<(String, LoraConfig)>>,
    adapter_safetensors: Option<Vec<(String, PathBuf)>>,
    classifier_path: Option<PathBuf>,
    xlora_order: Option<Ordering>,
    xlora_config: Option<XLoraConfig>,
}

fn get_xlora_paths(
    xlora_model_id: &Option<String>,
    token_source: &TokenSource,
    revision: String,
    xlora_order: &Option<Ordering>,
) -> Result<XLoraPaths> {
    Ok(if let Some(ref xlora_id) = xlora_model_id {
        let api = ApiBuilder::new()
            .with_progress(true)
            .with_token(Some(get_token(token_source)?))
            .build()?;
        let api = api.repo(Repo::with_revision(
            xlora_id.clone(),
            RepoType::Model,
            revision,
        ));
        let xlora_classifier = &api
            .info()?
            .siblings
            .iter()
            .map(|x| x.rfilename.clone())
            .filter(|x| x.contains("xlora_classifier.safetensors"))
            .collect::<Vec<_>>()[0];
        let xlora_config = &api
            .info()?
            .siblings
            .iter()
            .map(|x| x.rfilename.clone())
            .filter(|x| x.contains("xlora_config.json"))
            .collect::<Vec<_>>()[0];
        let classifier_path = api.get(xlora_classifier)?;
        let config_path = api.get(xlora_config)?;
        let conf = fs::read_to_string(config_path)?;
        let xlora_config: XLoraConfig = serde_json::from_str(&conf)?;

        let adapter_files = api
            .info()?
            .siblings
            .iter()
            .map(|x| x.rfilename.clone())
            .filter(|x| x.contains("/adapter_"))
            .map(|x| {
                let mut split = x.split('/');
                let pos = split.clone().count() - 2;
                let name = split.nth(pos).unwrap().to_string();
                (x, name)
            })
            .collect::<Vec<_>>();
        let mut adapters_paths: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for (file, name) in adapter_files {
            if let Some(paths) = adapters_paths.get_mut(&name) {
                paths.push(api.get(&file)?);
            } else {
                adapters_paths.insert(name, vec![api.get(&file)?]);
            }
        }
        let mut adapters_configs = Vec::new();
        let mut adapters_safetensors = Vec::new();
        let adapter_order = if let Some(ref a) = xlora_config.adapters {
            a.clone()
        } else {
            if xlora_order.as_ref().unwrap().adapters.is_none() {
                return Err(anyhow::Error::msg(
                    "Must specify adapters in ordering.".to_string(),
                ));
            }
            xlora_order
                .as_ref()
                .unwrap()
                .adapters
                .as_ref()
                .unwrap()
                .clone()
        };
        for name in &adapter_order {
            let paths = adapters_paths.get(name).unwrap();
            for path in paths {
                if path.extension().unwrap() == "safetensors" {
                    adapters_safetensors.push((name.clone(), path.to_owned()));
                } else {
                    let conf = fs::read_to_string(path)?;
                    let lora_config: LoraConfig = serde_json::from_str(&conf)?;
                    adapters_configs.push((name.clone(), lora_config));
                }
            }
        }
        XLoraPaths {
            adapter_configs: Some(adapters_configs),
            adapter_safetensors: Some(adapters_safetensors),
            classifier_path: Some(classifier_path),
            xlora_order: xlora_order.clone(),
            xlora_config: Some(xlora_config),
        }
    } else {
        XLoraPaths {
            adapter_configs: None,
            adapter_safetensors: None,
            classifier_path: None,
            xlora_order: None,
            xlora_config: None,
        }
    })
}

fn get_model_paths(
    revision: String,
    token_source: &TokenSource,
    quantized_model_id: &Option<String>,
    quantized_filename: &Option<String>,
    api: &ApiRepo,
) -> Result<Vec<PathBuf>> {
    match &quantized_filename {
        Some(name) => match quantized_model_id.as_ref().unwrap().as_str() {
            "" => Ok(vec![PathBuf::from_str(name).unwrap()]),
            id => {
                let qapi = ApiBuilder::new()
                    .with_progress(true)
                    .with_token(Some(get_token(token_source)?))
                    .build()?;
                let qapi = qapi.repo(Repo::with_revision(
                    id.to_string(),
                    RepoType::Model,
                    revision.clone(),
                ));
                Ok(vec![qapi.get(name).unwrap()])
            }
        },
        None => {
            let mut filenames = vec![];
            for rfilename in api
                .info()?
                .siblings
                .iter()
                .map(|x| x.rfilename.clone())
                .filter(|x| x.ends_with(".safetensors"))
            {
                let filename = api.get(&rfilename)?;
                filenames.push(filename);
            }
            Ok(filenames)
        }
    }
}

#[macro_export]
macro_rules! deserialize_chat_template {
    ($paths:expr, $this:ident) => {{
        let template: ChatTemplate = serde_json::from_str(&fs::read_to_string(
            $paths.get_template_filename(),
        )?).unwrap();
        #[derive(Debug, serde::Deserialize)]
        struct SpecifiedTemplate {
            chat_template: String,
            bos_token: Option<String>,
            eos_token: Option<String>,
        }
        match template.chat_template {
            Some(_) => template,
            None => {
                println!("`tokenizer_config.json` does not contain a chat template, attempting to use specified JINJA chat template.");
                let mut deser: HashMap<String, Value> =
                    serde_json::from_str(&fs::read_to_string($paths.get_template_filename())?)
                        .unwrap();
                match $this.chat_template.clone() {
                    Some(t) => {
                        if t.ends_with(".json") {
                            println!("Loading specified loading chat template file at `{t}`.");
                            let templ: SpecifiedTemplate = serde_json::from_str(&fs::read_to_string(t.clone())?).unwrap();
                            deser.insert(
                                "chat_template".to_string(),
                                Value::String(templ.chat_template),
                            );
                            if templ.bos_token.is_some() {
                                deser.insert(
                                    "bos_token".to_string(),
                                    Value::String(templ.bos_token.unwrap()),
                                );
                            }
                            if templ.eos_token.is_some() {
                                deser.insert(
                                    "eos_token".to_string(),
                                    Value::String(templ.eos_token.unwrap()),
                                );
                            }
                            println!("Loaded chat template file.");
                        } else {
                            deser.insert(
                                "chat_template".to_string(),
                                Value::String(t),
                            );
                            println!("Loaded specified literal chat template.");
                        }
                    },
                    None => {
                        println!("No specified chat template, loading default chat template at `./default.json`.");
                        deser.insert(
                            "chat_template".to_string(),
                            Value::String(fs::read_to_string("./default.json")?),
                        );
                        println!("Default chat template loaded.");
                    }
                };
                let ser = serde_json::to_string_pretty(&deser).unwrap();
                serde_json::from_str(&ser).unwrap()
            }
        }
    }};
}
