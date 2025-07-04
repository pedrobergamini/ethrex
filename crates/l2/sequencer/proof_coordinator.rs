use crate::sequencer::errors::{ConnectionHandlerError, ProofCoordinatorError};
use crate::sequencer::setup::{prepare_quote_prerequisites, register_tdx_key};
use crate::sequencer::utils::get_latest_sent_batch;
use crate::utils::prover::proving_systems::{BatchProof, ProverType};
use crate::utils::prover::save_state::{
    StateFileType, StateType, batch_number_has_state_file, write_state,
};
use crate::{
    BlockProducerConfig, CommitterConfig, EthConfig, ProofCoordinatorConfig, SequencerConfig,
};
use bytes::Bytes;
use ethrex_blockchain::Blockchain;
use ethrex_common::types::BlobsBundle;
use ethrex_common::types::block_execution_witness::ExecutionWitnessResult;
use ethrex_common::{
    Address,
    types::{Block, blobs_bundle},
};
use ethrex_rpc::clients::eth::EthClient;
use ethrex_storage::Store;
use ethrex_storage_rollup::StoreRollup;
use secp256k1::SecretKey;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use spawned_concurrency::{CallResponse, CastResponse, GenServer};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tracing::{debug, error, info, warn};

#[serde_as]
#[derive(Serialize, Deserialize)]
pub struct ProverInputData {
    pub blocks: Vec<Block>,
    pub db: ExecutionWitnessResult,
    pub elasticity_multiplier: u64,
    #[cfg(feature = "l2")]
    #[serde_as(as = "[_; 48]")]
    pub blob_commitment: blobs_bundle::Commitment,
    #[cfg(feature = "l2")]
    #[serde_as(as = "[_; 48]")]
    pub blob_proof: blobs_bundle::Proof,
}

/// Enum for the ProverServer <--> ProverClient Communication Protocol.
#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize)]
pub enum ProofData {
    /// 1.
    /// The client performs any needed setup steps
    /// This includes things such as key registration
    ProverSetup {
        prover_type: ProverType,
        payload: Bytes,
    },

    /// 2.
    /// The Server acknowledges the receipt of the setup and it's completion
    ProverSetupACK,

    /// 3.
    /// The Client initiates the connection with a BatchRequest.
    /// Asking for the ProverInputData the prover_server considers/needs.
    BatchRequest,

    /// 4.
    /// The Server responds with a BatchResponse containing the ProverInputData.
    /// If the BatchResponse is ProofData::BatchResponse{None, None},
    /// the Client knows the BatchRequest couldn't be performed.
    BatchResponse {
        batch_number: Option<u64>,
        input: Option<ProverInputData>,
    },

    /// 5.
    /// The Client submits the zk Proof generated by the prover for the specified batch.
    ProofSubmit {
        batch_number: u64,
        batch_proof: BatchProof,
    },

    /// 6.
    /// The Server acknowledges the receipt of the proof and updates its state,
    ProofSubmitACK { batch_number: u64 },
}

impl ProofData {
    /// Builder function for creating a ProofSubmitAck
    pub fn prover_setup(prover_type: ProverType, payload: Bytes) -> Self {
        ProofData::ProverSetup {
            prover_type,
            payload,
        }
    }

    /// Builder function for creating a ProofSubmitAck
    pub fn prover_setup_ack() -> Self {
        ProofData::ProverSetupACK
    }

    /// Builder function for creating a BatchRequest
    pub fn batch_request() -> Self {
        ProofData::BatchRequest
    }

    /// Builder function for creating a BlockResponse
    pub fn batch_response(batch_number: u64, input: ProverInputData) -> Self {
        ProofData::BatchResponse {
            batch_number: Some(batch_number),
            input: Some(input),
        }
    }

    pub fn empty_batch_response() -> Self {
        ProofData::BatchResponse {
            batch_number: None,
            input: None,
        }
    }

    /// Builder function for creating a ProofSubmit
    pub fn proof_submit(batch_number: u64, batch_proof: BatchProof) -> Self {
        ProofData::ProofSubmit {
            batch_number,
            batch_proof,
        }
    }

    /// Builder function for creating a ProofSubmitAck
    pub fn proof_submit_ack(batch_number: u64) -> Self {
        ProofData::ProofSubmitACK { batch_number }
    }
}

#[derive(Clone)]
pub struct ProofCoordinatorState {
    listen_ip: IpAddr,
    port: u16,
    store: Store,
    eth_client: EthClient,
    on_chain_proposer_address: Address,
    elasticity_multiplier: u64,
    rollup_store: StoreRollup,
    rpc_url: String,
    l1_private_key: SecretKey,
    blockchain: Arc<Blockchain>,
    validium: bool,
    needed_proof_types: Vec<ProverType>,
}

impl ProofCoordinatorState {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        config: &ProofCoordinatorConfig,
        committer_config: &CommitterConfig,
        eth_config: &EthConfig,
        proposer_config: &BlockProducerConfig,
        store: Store,
        rollup_store: StoreRollup,
        blockchain: Arc<Blockchain>,
        needed_proof_types: Vec<ProverType>,
    ) -> Result<Self, ProofCoordinatorError> {
        let eth_client = EthClient::new_with_config(
            eth_config.rpc_url.iter().map(AsRef::as_ref).collect(),
            eth_config.max_number_of_retries,
            eth_config.backoff_factor,
            eth_config.min_retry_delay,
            eth_config.max_retry_delay,
            Some(eth_config.maximum_allowed_max_fee_per_gas),
            Some(eth_config.maximum_allowed_max_fee_per_blob_gas),
        )?;
        let on_chain_proposer_address = committer_config.on_chain_proposer_address;

        let rpc_url = eth_config
            .rpc_url
            .first()
            .ok_or(ProofCoordinatorError::Custom(
                "no rpc urls present!".to_string(),
            ))?
            .to_string();

        Ok(Self {
            listen_ip: config.listen_ip,
            port: config.listen_port,
            store,
            eth_client,
            on_chain_proposer_address,
            elasticity_multiplier: proposer_config.elasticity_multiplier,
            rollup_store,
            rpc_url,
            l1_private_key: config.l1_private_key,
            blockchain,
            validium: config.validium,
            needed_proof_types,
        })
    }
}

pub enum ProofCordInMessage {
    Listen { listener: TcpListener },
}

#[derive(Clone, PartialEq)]
pub enum ProofCordOutMessage {
    Done,
}

pub struct ProofCoordinator;

impl ProofCoordinator {
    pub async fn spawn(
        store: Store,
        rollup_store: StoreRollup,
        cfg: SequencerConfig,
        blockchain: Arc<Blockchain>,
        needed_proof_types: Vec<ProverType>,
    ) -> Result<(), ProofCoordinatorError> {
        let state = ProofCoordinatorState::new(
            &cfg.proof_coordinator,
            &cfg.l1_committer,
            &cfg.eth,
            &cfg.block_producer,
            store,
            rollup_store,
            blockchain,
            needed_proof_types,
        )
        .await?;
        let listener = TcpListener::bind(format!("{}:{}", state.listen_ip, state.port)).await?;
        let mut proof_coordinator = ProofCoordinator::start(state);
        let _ = proof_coordinator
            .cast(ProofCordInMessage::Listen { listener })
            .await;
        Ok(())
    }
}

impl GenServer for ProofCoordinator {
    type InMsg = ProofCordInMessage;
    type OutMsg = ProofCordOutMessage;
    type State = ProofCoordinatorState;
    type Error = ProofCoordinatorError;

    fn new() -> Self {
        Self {}
    }

    async fn handle_call(
        &mut self,
        _message: Self::InMsg,
        _tx: &spawned_rt::mpsc::Sender<spawned_concurrency::GenServerInMsg<Self>>,
        _state: &mut Self::State,
    ) -> CallResponse<Self::OutMsg> {
        CallResponse::Reply(ProofCordOutMessage::Done)
    }

    async fn handle_cast(
        &mut self,
        message: Self::InMsg,
        _tx: &spawned_rt::mpsc::Sender<spawned_concurrency::GenServerInMsg<Self>>,
        state: &mut Self::State,
    ) -> CastResponse {
        info!("Receiving message");
        match message {
            ProofCordInMessage::Listen { listener } => {
                handle_listens(state, listener).await;
            }
        }
        CastResponse::Stop
    }
}

async fn handle_listens(state: &ProofCoordinatorState, listener: TcpListener) {
    info!("Starting TCP server at {}:{}.", state.listen_ip, state.port);
    loop {
        let res = listener.accept().await;
        match res {
            Ok((stream, addr)) => {
                // Cloning the ProofCoordinatorState structure to use the handle_connection() fn
                // in every spawned task.
                // The important fields are `Store` and `EthClient`
                // Both fields are wrapped with an Arc, making it possible to clone
                // the entire structure.
                let _ = ConnectionHandler::spawn(state.clone(), stream, addr)
                    .await
                    .inspect_err(|err| {
                        error!("Error starting ConnectionHandler: {err}");
                    });
            }
            Err(e) => {
                error!("Failed to accept connection: {e}");
            }
        }

        debug!("Connection closed");
    }
}

struct ConnectionHandler;

impl ConnectionHandler {
    async fn spawn(
        state: ProofCoordinatorState,
        stream: TcpStream,
        addr: SocketAddr,
    ) -> Result<(), ConnectionHandlerError> {
        let mut connection_handler = ConnectionHandler::start(state);
        connection_handler
            .cast(ConnInMessage::Connection { stream, addr })
            .await
            .map_err(ConnectionHandlerError::GenServerError)
    }
}

pub enum ConnInMessage {
    Connection { stream: TcpStream, addr: SocketAddr },
}

#[derive(Clone, PartialEq)]
pub enum ConnOutMessage {
    Done,
}

impl GenServer for ConnectionHandler {
    type InMsg = ConnInMessage;
    type OutMsg = ConnOutMessage;
    type State = ProofCoordinatorState;
    type Error = ProofCoordinatorError;

    fn new() -> Self {
        Self {}
    }

    async fn handle_call(
        &mut self,
        _message: Self::InMsg,
        _tx: &spawned_rt::mpsc::Sender<spawned_concurrency::GenServerInMsg<Self>>,
        _state: &mut Self::State,
    ) -> CallResponse<Self::OutMsg> {
        CallResponse::Reply(ConnOutMessage::Done)
    }

    async fn handle_cast(
        &mut self,
        message: Self::InMsg,
        _tx: &spawned_rt::mpsc::Sender<spawned_concurrency::GenServerInMsg<Self>>,
        state: &mut Self::State,
    ) -> CastResponse {
        info!("Receiving message");
        match message {
            ConnInMessage::Connection { stream, addr } => {
                if let Err(err) = handle_connection(state, stream).await {
                    error!("Error handling connection from {addr}: {err}");
                } else {
                    debug!("Connection from {addr} handled successfully");
                }
            }
        }
        CastResponse::Stop
    }
}

async fn handle_connection(
    state: &ProofCoordinatorState,
    mut stream: TcpStream,
) -> Result<(), ProofCoordinatorError> {
    let mut buffer = Vec::new();
    stream.read_to_end(&mut buffer).await?;

    let data: Result<ProofData, _> = serde_json::from_slice(&buffer);
    match data {
        Ok(ProofData::BatchRequest) => {
            if let Err(e) = handle_request(state, &mut stream).await {
                error!("Failed to handle BatchRequest: {e}");
            }
        }
        Ok(ProofData::ProofSubmit {
            batch_number,
            batch_proof,
        }) => {
            if let Err(e) = handle_submit(&mut stream, batch_number, batch_proof).await {
                error!("Failed to handle ProofSubmit: {e}");
            }
        }
        Ok(ProofData::ProverSetup {
            prover_type,
            payload,
        }) => {
            if let Err(e) = handle_setup(state, &mut stream, prover_type, payload).await {
                error!("Failed to handle ProverSetup: {e}");
            }
        }
        Err(e) => {
            warn!("Failed to parse request: {e}");
        }
        _ => {
            warn!("Invalid request");
        }
    }

    debug!("Connection closed");
    Ok(())
}

async fn handle_request(
    state: &ProofCoordinatorState,
    stream: &mut TcpStream,
) -> Result<(), ProofCoordinatorError> {
    info!("BatchRequest received");

    let batch_to_verify = 1 + get_latest_sent_batch(
        state.needed_proof_types.clone(),
        &state.rollup_store,
        &state.eth_client,
        state.on_chain_proposer_address,
    )
    .await
    .map_err(|err| ProofCoordinatorError::InternalError(err.to_string()))?;

    let response = if !state.rollup_store.contains_batch(&batch_to_verify).await? {
        let response = ProofData::empty_batch_response();
        debug!("Sending empty BatchResponse");
        response
    } else {
        let input = create_prover_input(state, batch_to_verify).await?;
        let response = ProofData::batch_response(batch_to_verify, input);
        debug!("Sending BatchResponse for block_number: {batch_to_verify}");
        response
    };

    let buffer = serde_json::to_vec(&response)?;
    stream
        .write_all(&buffer)
        .await
        .map_err(ProofCoordinatorError::ConnectionError)
        .map(|_| info!("BatchResponse sent for batch number: {batch_to_verify}"))
}

async fn handle_submit(
    stream: &mut TcpStream,
    batch_number: u64,
    batch_proof: BatchProof,
) -> Result<(), ProofCoordinatorError> {
    info!("ProofSubmit received for batch number: {batch_number}");

    // Check if we have the proof for that ProverType
    if batch_number_has_state_file(
        StateFileType::BatchProof(batch_proof.prover_type()),
        batch_number,
    )? {
        debug!("Already known proof. Skipping");
    } else {
        write_state(batch_number, &StateType::BatchProof(batch_proof))?;
    }

    let response = ProofData::proof_submit_ack(batch_number);

    let buffer = serde_json::to_vec(&response)?;
    stream
        .write_all(&buffer)
        .await
        .map_err(ProofCoordinatorError::ConnectionError)
        .map(|_| info!("ProofSubmit ACK sent"))
}

async fn handle_setup(
    state: &ProofCoordinatorState,
    stream: &mut TcpStream,
    prover_type: ProverType,
    payload: Bytes,
) -> Result<(), ProofCoordinatorError> {
    info!("ProverSetup received for {prover_type}");

    match prover_type {
        ProverType::TDX => {
            prepare_quote_prerequisites(
                &state.eth_client,
                &state.rpc_url,
                &hex::encode(state.l1_private_key.as_ref()),
                &hex::encode(&payload),
            )
            .await
            .map_err(|e| ProofCoordinatorError::Custom(format!("Could not setup TDX key {e}")))?;
            register_tdx_key(
                &state.eth_client,
                &state.l1_private_key,
                state.on_chain_proposer_address,
                payload,
            )
            .await?;
        }
        _ => {
            warn!("Setup requested for {prover_type}, which doesn't need setup.")
        }
    }

    let response = ProofData::prover_setup_ack();

    let buffer = serde_json::to_vec(&response)?;
    stream
        .write_all(&buffer)
        .await
        .map_err(ProofCoordinatorError::ConnectionError)
        .map(|_| info!("ProverSetupACK sent"))
}

async fn create_prover_input(
    state: &ProofCoordinatorState,
    batch_number: u64,
) -> Result<ProverInputData, ProofCoordinatorError> {
    // Get blocks in batch
    let Some(block_numbers) = state
        .rollup_store
        .get_block_numbers_by_batch(batch_number)
        .await?
    else {
        return Err(ProofCoordinatorError::ItemNotFoundInStore(format!(
            "Batch number {batch_number} not found in store"
        )));
    };

    let blocks = fetch_blocks(state, block_numbers).await?;

    let witness = state
        .blockchain
        .generate_witness_for_blocks(&blocks)
        .await
        .map_err(ProofCoordinatorError::from)?;

    // Get blobs bundle cached by the L1 Committer (blob, commitment, proof)
    let (blob_commitment, blob_proof) = if state.validium {
        ([0; 48], [0; 48])
    } else {
        let blob = state
            .rollup_store
            .get_blobs_by_batch(batch_number)
            .await?
            .ok_or(ProofCoordinatorError::MissingBlob(batch_number))?;
        let BlobsBundle {
            mut commitments,
            mut proofs,
            ..
        } = BlobsBundle::create_from_blobs(&blob)?;
        match (commitments.pop(), proofs.pop()) {
            (Some(commitment), Some(proof)) => (commitment, proof),
            _ => return Err(ProofCoordinatorError::MissingBlob(batch_number)),
        }
    };

    debug!("Created prover input for batch {batch_number}");

    Ok(ProverInputData {
        db: witness,
        blocks,
        elasticity_multiplier: state.elasticity_multiplier,
        #[cfg(feature = "l2")]
        blob_commitment,
        #[cfg(feature = "l2")]
        blob_proof,
    })
}

async fn fetch_blocks(
    state: &ProofCoordinatorState,
    block_numbers: Vec<u64>,
) -> Result<Vec<Block>, ProofCoordinatorError> {
    let mut blocks = vec![];
    for block_number in block_numbers {
        let header = state
            .store
            .get_block_header(block_number)?
            .ok_or(ProofCoordinatorError::StorageDataIsNone)?;
        let body = state
            .store
            .get_block_body(block_number)
            .await?
            .ok_or(ProofCoordinatorError::StorageDataIsNone)?;
        blocks.push(Block::new(header, body));
    }
    Ok(blocks)
}
