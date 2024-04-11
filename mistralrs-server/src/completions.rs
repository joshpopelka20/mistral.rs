use std::{
    env,
    error::Error,
    pin::Pin,
    sync::{
        mpsc::{channel, Receiver, Sender},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};

use crate::openai::{CompletionRequest, Grammar, StopTokens};
use anyhow::Result;
use axum::{
    extract::{Json, State},
    http::{self, StatusCode},
    response::{
        sse::{Event, KeepAlive},
        IntoResponse, Sse,
    },
};
use either::Either;
use mistralrs_core::{
    CompletionResponse, Constraint, MistralRs, Request, RequestType, Response, SamplingParams,
    StopTokens as InternalStopTokens,
};
use serde::Serialize;

#[derive(Debug)]
struct ModelErrorMessage(String);
impl std::fmt::Display for ModelErrorMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ModelErrorMessage {}
pub struct Streamer {
    rx: Receiver<Response>,
    is_done: bool,
    state: Arc<MistralRs>,
}

impl futures::Stream for Streamer {
    type Item = Result<Event, axum::Error>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.is_done {
            return Poll::Ready(None);
        }
        match self.rx.try_recv() {
            Ok(resp) => match resp {
                Response::CompletionModelError(msg, _) => {
                    MistralRs::maybe_log_error(
                        self.state.clone(),
                        &ModelErrorMessage(msg.to_string()),
                    );
                    Poll::Ready(Some(Ok(Event::default().data(msg))))
                }
                Response::ValidationError(e) => {
                    Poll::Ready(Some(Ok(Event::default().data(e.to_string()))))
                }
                Response::InternalError(e) => {
                    MistralRs::maybe_log_error(self.state.clone(), &*e);
                    Poll::Ready(Some(Ok(Event::default().data(e.to_string()))))
                }
                Response::CompletionChunk(response) => {
                    if response.done {
                        self.is_done = true;
                    }
                    MistralRs::maybe_log_response(self.state.clone(), &response);
                    Poll::Ready(Some(Ok(Event::default().data(response.data))))
                }
                Response::CompletionDone(_) => unreachable!(),
                Response::Chunk(_) => unreachable!(),
                Response::Done(_) => unreachable!(),
                Response::ModelError(_, _) => unreachable!(),
            },
            Err(_) => Poll::Pending,
        }
    }
}

pub enum CompletionResponder {
    Sse(Sse<Streamer>),
    Json(CompletionResponse),
    ModelError(String, CompletionResponse),
    InternalError(Box<dyn Error>),
    ValidationError(Box<dyn Error>),
}

trait ErrorToResponse: Serialize {
    fn to_response(&self, code: StatusCode) -> axum::response::Response {
        let mut r = Json(self).into_response();
        *r.status_mut() = code;
        r
    }
}

#[derive(Serialize)]
struct JsonError {
    message: String,
}

impl JsonError {
    fn new(message: String) -> Self {
        Self { message }
    }
}
impl ErrorToResponse for JsonError {}

#[derive(Serialize)]
struct JsonModelError {
    message: String,
    partial_response: CompletionResponse,
}

impl JsonModelError {
    fn new(message: String, partial_response: CompletionResponse) -> Self {
        Self {
            message,
            partial_response,
        }
    }
}

impl ErrorToResponse for JsonModelError {}

impl IntoResponse for CompletionResponder {
    fn into_response(self) -> axum::response::Response {
        match self {
            CompletionResponder::Sse(s) => s.into_response(),
            CompletionResponder::Json(s) => Json(s).into_response(),
            CompletionResponder::InternalError(e) => {
                JsonError::new(e.to_string()).to_response(http::StatusCode::INTERNAL_SERVER_ERROR)
            }
            CompletionResponder::ValidationError(e) => {
                JsonError::new(e.to_string()).to_response(http::StatusCode::UNPROCESSABLE_ENTITY)
            }
            CompletionResponder::ModelError(msg, response) => JsonModelError::new(msg, response)
                .to_response(http::StatusCode::INTERNAL_SERVER_ERROR),
        }
    }
}

fn parse_request(
    oairequest: CompletionRequest,
    state: Arc<MistralRs>,
    tx: Sender<Response>,
) -> Request {
    let repr = serde_json::to_string(&oairequest).unwrap();
    MistralRs::maybe_log_request(state.clone(), repr);

    let stop_toks = match oairequest.stop_seqs {
        Some(StopTokens::Multi(m)) => Some(InternalStopTokens::Seqs(m)),
        Some(StopTokens::Single(s)) => Some(InternalStopTokens::Seqs(vec![s])),
        Some(StopTokens::MultiId(m)) => Some(InternalStopTokens::Ids(m)),
        Some(StopTokens::SingleId(s)) => Some(InternalStopTokens::Ids(vec![s])),
        None => None,
    };

    Request {
        id: state.next_request_id(),
        messages: Either::Right(oairequest.prompt),
        sampling_params: SamplingParams {
            temperature: oairequest.temperature,
            top_k: oairequest.top_k,
            top_p: oairequest.top_p,
            top_n_logprobs: oairequest.logprobs.unwrap_or(1),
            frequency_penalty: oairequest.frequency_penalty,
            presence_penalty: oairequest.presence_penalty,
            max_len: oairequest.max_tokens,
            stop_toks,
            logits_bias: oairequest.logit_bias,
            n_choices: oairequest.n_choices,
        },
        response: tx,
        return_logprobs: oairequest.logprobs.is_some(),
        is_streaming: oairequest.stream.unwrap_or(false),

        constraint: match oairequest.grammar {
            Some(Grammar::Yacc(yacc)) => Constraint::Yacc(yacc),
            Some(Grammar::Regex(regex)) => Constraint::Regex(regex),
            None => Constraint::None,
        },

        request_type: RequestType::Completion,
    }
}

#[utoipa::path(
    post,
    tag = "Mistral.rs",
    path = "/v1/completions",
    request_body = CompletionRequest,
    responses((status = 200, description = "Completions"))
)]
pub async fn completions(
    State(state): State<Arc<MistralRs>>,
    Json(oairequest): Json<CompletionRequest>,
) -> CompletionResponder {
    let (tx, rx) = channel();
    let request = parse_request(oairequest, state.clone(), tx);
    let is_streaming = request.is_streaming;
    let sender = state.get_sender();
    sender.send(request).unwrap();

    if is_streaming {
        let streamer = Streamer {
            rx,
            is_done: false,
            state,
        };

        CompletionResponder::Sse(
            Sse::new(streamer).keep_alive(
                KeepAlive::new()
                    .interval(Duration::from_millis(
                        env::var("KEEP_ALIVE_INTERVAL")
                            .map(|val| val.parse::<u64>().unwrap_or(1000))
                            .unwrap_or(1000),
                    ))
                    .text("keep-alive-text"),
            ),
        )
    } else {
        let response = rx.recv().unwrap();

        match response {
            Response::InternalError(e) => {
                MistralRs::maybe_log_error(state, &*e);
                CompletionResponder::InternalError(e)
            }
            Response::CompletionModelError(msg, response) => {
                MistralRs::maybe_log_error(state.clone(), &ModelErrorMessage(msg.to_string()));
                MistralRs::maybe_log_response(state, &response);
                CompletionResponder::ModelError(msg, response)
            }
            Response::ValidationError(e) => CompletionResponder::ValidationError(e),
            Response::CompletionDone(response) => {
                MistralRs::maybe_log_response(state, &response);
                CompletionResponder::Json(response)
            }
            Response::CompletionChunk(_) => unreachable!(),
            Response::Chunk(_) => unreachable!(),
            Response::Done(_) => unreachable!(),
            Response::ModelError(_, _) => unreachable!(),
        }
    }
}