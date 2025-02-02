use cosmwasm_std::{
    entry_point, to_binary, Addr, Binary, Deps, DepsMut, Env, MessageInfo, Response, StdError,
    StdResult,
};
use secret_toolkit::{
    crypto::secp256k1::{PrivateKey, PublicKey},
    crypto::{sha_256, Prng},
    utils::{pad_handle_result, pad_query_result, HandleCallback},
};

use crate::{
    msg::{
        ExecuteMsg, InputResponse, InstantiateMsg, PostExecutionMsg, PreExecutionMsg,
        PublicKeyResponse, QueryMsg, ResponseStatus::Success, SecretMsg,
    },
    state::{KeyPair, State, TaskInfo, CONFIG, CREATOR, MY_ADDRESS, PRNG_SEED, TASK_MAP},
    PrivContractHandleMsg,
};

use hex::ToHex;
use sha3::{Digest, Keccak256};

/// pad handle responses and log attributes to blocks of 256 bytes to prevent leaking info based on
/// response size
pub const BLOCK_SIZE: usize = 256;

#[cfg(feature = "contract")]
////////////////////////////////////// Init ///////////////////////////////////////
/// Returns InitResult
///
/// Initializes the contract
///
/// # Arguments
///
/// * `deps` - mutable reference to Extern containing all the contract's external dependencies
/// * `env` - Env of contract's environment
/// * `msg` - InitMsg passed in with the instantiation message
#[entry_point]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response> {
    // Save this contract's address
    let my_address_raw = &deps.api.addr_canonicalize(env.contract.address.as_str())?;
    MY_ADDRESS.save(deps.storage, my_address_raw)?;

    // Save the address of the contract's creator
    let creator_raw = deps.api.addr_canonicalize(info.sender.as_str())?;
    CREATOR.save(deps.storage, &creator_raw)?;

    // Set admin address if provided, or else use creator address
    let admin_raw = msg
        .admin
        .map(|a| deps.api.addr_canonicalize(a.as_str()))
        .transpose()?
        .unwrap_or(creator_raw);

    // Save both key pairs
    let state = State {
        admin: admin_raw,
        keyed: false,
        tx_cnt: 0,
        encryption_keys: KeyPair::default(),
        signing_keys: KeyPair::default(),
    };

    CONFIG.save(deps.storage, &state)?;

    // create a message to request randomness from scrt-rng oracle
    let rng_msg = SecretMsg::CreateRn {
        cb_msg: Binary(vec![]),
        entropy: msg.entropy,
        max_blk_delay: None,
        purpose: Some("secret gateway entropy".to_string()),
        receiver_addr: Some(env.contract.address),
        receiver_code_hash: env.contract.code_hash,
    }
    .to_cosmos_msg(msg.rng_hash, msg.rng_addr.into_string(), None)?;

    Ok(Response::new().add_message(rng_msg))
}

#[cfg(feature = "contract")]
///////////////////////////////////// Handle //////////////////////////////////////
/// Returns HandleResult
///
/// # Arguments
///
/// * `deps` - mutable reference to Extern containing all the contract's external dependencies
/// * `env` - Env of contract's environment
/// * `msg` - HandleMsg passed in with the execute message
#[entry_point]
pub fn execute(
    deps: DepsMut,
    env: Env,
    _info: MessageInfo,
    msg: ExecuteMsg,
) -> StdResult<Response> {
    match msg {
        ExecuteMsg::KeyGen { rng_hash, rng_addr } => {
            pad_handle_result(try_fulfill_rn(deps, env, rng_hash, rng_addr), BLOCK_SIZE)
        }
        ExecuteMsg::ReceiveFRn {
            cb_msg: _,
            purpose: _,
            rn,
        } => pad_handle_result(create_gateway_keys(deps, env, rn), BLOCK_SIZE),
        ExecuteMsg::Input { inputs } => {
            pad_handle_result(pre_execution(deps, env, inputs), BLOCK_SIZE)
        }
        ExecuteMsg::Output { outputs } => post_execution(deps, env, outputs),
    }
}

fn try_fulfill_rn(
    deps: DepsMut,
    env: Env,
    rng_hash: String,
    rng_addr: Addr,
) -> StdResult<Response> {
    // load config
    let state = CONFIG.load(deps.storage)?;

    // check if the keys have already been created
    if state.keyed {
        return Err(StdError::generic_err(
            "keys have already been created".to_string(),
        ));
    }

    let fulfill_rn_msg = SecretMsg::FulfillRn {
        creator_addr: env.contract.address,
        purpose: Some("secret gateway entropy".to_string()),
        receiver_code_hash: env.contract.code_hash,
    }
    .to_cosmos_msg(rng_hash, rng_addr.into_string(), None)?;

    Ok(Response::new().add_message(fulfill_rn_msg))
}

fn create_gateway_keys(deps: DepsMut, env: Env, prng_seed: [u8; 32]) -> StdResult<Response> {
    // load config
    let state = CONFIG.load(deps.storage)?;

    // check if the keys have already been created
    if state.keyed {
        return Err(StdError::generic_err(
            "keys have already been created".to_string(),
        ));
    }

    // Generate secp256k1 key pair for encryption
    let (secret, public, new_prng_seed) = generate_keypair(&env, prng_seed.to_vec(), None)?;
    let encryption_keys = KeyPair {
        sk: Binary(secret.serialize().to_vec()), // private key is 32 bytes,
        pk: Binary(public.serialize_compressed().to_vec()), // public key is 33 bytes
    };

    // Generate secp256k1 key pair for signing messages
    let (secret, public, new_prng_seed) = generate_keypair(&env, new_prng_seed, None)?;
    let signing_keys = KeyPair {
        sk: Binary(secret.serialize().to_vec()), // private key is 32 bytes,
        pk: Binary(public.serialize().to_vec()), // public key is 65 bytes
    };

    CONFIG.update(deps.storage, |mut state| {
        state.keyed = true;
        state.encryption_keys = encryption_keys.clone();
        state.signing_keys = signing_keys.clone();
        Ok(state)
    })?;

    PRNG_SEED.save(deps.storage, &new_prng_seed)?; // is there any need to save this?

    let encryption_pubkey = encryption_keys.pk.to_base64();
    let signing_pubkey = signing_keys.pk.to_base64();

    Ok(Response::new()
        .add_attribute_plaintext("encryption_pubkey", encryption_pubkey)
        .add_attribute_plaintext("signing_pubkey", signing_pubkey))
}

fn pre_execution(deps: DepsMut, _env: Env, msg: PreExecutionMsg) -> StdResult<Response> {
    // verify that signature is correct
    msg.verify(&deps)?;

    // load config
    let config = CONFIG.load(deps.storage)?;

    // decrypt payload
    let payload = msg.decrypt_payload(config.encryption_keys.sk)?;
    let input_values = payload.data;

    // combine input values and task ID to create verification hash
    let input_hash = sha_256(&[input_values.as_bytes(), &msg.task_id.to_le_bytes()].concat());

    // verify the internal verification key matches the user address
    if payload.user_key != msg.user_key {
        return Err(StdError::generic_err("verification key mismatch"));
    }
    // verify the routing info matches the internally stored routing info
    if msg.routing_info != payload.routing_info {
        return Err(StdError::generic_err("routing info mismatch"));
    }

    // create a task information store
    let task_info = TaskInfo {
        payload: msg.payload, // storing the ENCRYPTED payload
        payload_hash: msg.payload_hash,
        input_hash, // storing the DECRYPTED input_values hashed together with task ID
        source_network: msg.source_network,
        user_address: payload.user_address.clone(),
    };

    // map task ID to task info
    TASK_MAP.insert(deps.storage, &msg.task_id, &task_info)?;

    // load this gateway's signing key
    let mut signing_key_bytes = [0u8; 32];
    signing_key_bytes.copy_from_slice(config.signing_keys.sk.as_slice());

    // used in production to create signature
    #[cfg(target_arch = "wasm32")]
    let signature = deps
        .api
        .secp256k1_sign(&input_hash, &signing_key_bytes)
        .map_err(|err| StdError::generic_err(err.to_string()))?;
    // let signature = PrivateKey::parse(&signing_key_bytes)?
    //     .sign(&input_hash, deps.api)
    //     .serialize()
    //     .to_vec();

    // used only in unit testing to create signatures
    #[cfg(not(target_arch = "wasm32"))]
    let signature = {
        let secp = secp256k1::Secp256k1::signing_only();
        let sk = secp256k1::SecretKey::from_slice(&signing_key_bytes).unwrap();
        let message = secp256k1::Message::from_slice(&input_hash)
            .map_err(|err| StdError::generic_err(err.to_string()))?;
        secp.sign_ecdsa(&message, &sk).serialize_compact().to_vec()
    };

    // construct the message to send to the destination contract
    let private_contract_msg = SecretMsg::Input {
        message: PrivContractHandleMsg {
            input_values,
            handle: msg.handle,
            user_address: payload.user_address,
            task_id: msg.task_id,
            input_hash: Binary(input_hash.to_vec()),
            signature: Binary(signature),
        },
    };
    let cosmos_msg = private_contract_msg.to_cosmos_msg(
        msg.routing_code_hash,
        msg.routing_info.into_string(),
        None,
    )?;

    Ok(Response::new()
        .add_message(cosmos_msg)
        .add_attribute_plaintext("task_id", msg.task_id.to_string())
        .add_attribute_plaintext("status", "sent to private contract")
        .set_data(to_binary(&InputResponse { status: Success })?))
}

fn post_execution(deps: DepsMut, _env: Env, msg: PostExecutionMsg) -> StdResult<Response> {
    // load task info and remove task ID from map
    let task_info = TASK_MAP
        .get(deps.storage, &msg.task_id)
        .ok_or_else(|| StdError::generic_err("task id not found"))?;

    // this panics in unit tests
    #[cfg(target_arch = "wasm32")]
    TASK_MAP.remove(deps.storage, &msg.task_id)?;

    // verify that input hash is correct one for Task ID
    if msg.input_hash.as_slice() != task_info.input_hash.to_vec() {
        return Err(StdError::generic_err("input hash does not match task id"));
    }

    // rename for clarity (original source network is now the routing destination)
    let routing_info = task_info.source_network;

    // "hasher" is used to perform multiple Keccak256 hashes
    let mut hasher = Keccak256::new();

    // requirement of Ethereum's `ecrecover` function
    let prefix = "\x19Ethereum Signed Message:\n32".as_bytes();

    // the first hash guarantees the message lenth is 32
    // the second hash prepends the Ethereum message

    // create message hash of (result + payload + inputs)
    let data = [
        msg.result.as_bytes(),
        task_info.payload.as_slice(),
        &task_info.input_hash,
    ]
    .concat();
    hasher.update(&data);
    let result_hash = hasher.finalize_reset();
    hasher.update([prefix, &result_hash].concat());
    let result_hash = hasher.finalize_reset();

    // load this gateway's signing key
    let private_key = CONFIG.load(deps.storage)?.signing_keys.sk;
    let mut signing_key_bytes = [0u8; 32];
    signing_key_bytes.copy_from_slice(private_key.as_slice());

    // used in production to create signatures
    // NOTE: api.secp256k1_sign() will perform an additional sha_256 hash operation on the given data
    #[cfg(target_arch = "wasm32")]
    let result_signature = {
        // let sk = PrivateKey::parse(&signing_key_bytes)?;
        // let result_signature = sk.sign(&result_hash, deps.api).serialize().to_vec();

        let result_signature = deps
            .api
            .secp256k1_sign(&result_hash, &signing_key_bytes)
            .map_err(|err| StdError::generic_err(err.to_string()))?;

        result_signature
    };

    // used only in unit testing to create signatures
    #[cfg(not(target_arch = "wasm32"))]
    let result_signature = {
        let secp = secp256k1::Secp256k1::signing_only();
        let sk = secp256k1::SecretKey::from_slice(&signing_key_bytes).unwrap();

        let result_message = secp256k1::Message::from_slice(&result_hash)
            .map_err(|err| StdError::generic_err(err.to_string()))?;
        let result_signature = secp
            .sign_ecdsa_recoverable(&result_message, &sk)
            .serialize_compact();

        result_signature.1
    };

    // create hash of entire packet (used to verify the message wasn't modified in transit)
    let data = [
        "secret".as_bytes(),               // source network
        routing_info.as_bytes(),           // task_destination_network
        &msg.task_id.to_le_bytes(),        // task ID
        task_info.payload.as_slice(),      // payload (original encrypted payload)
        task_info.payload_hash.as_slice(), // original payload message
        msg.result.as_bytes(),             // result
        &result_hash,                      // result message
        &result_signature,                 // result signature
    ]
    .concat();
    hasher.update(&data);
    let packet_hash = hasher.finalize_reset();
    hasher.update([prefix, &packet_hash].concat());
    let packet_hash = hasher.finalize();

    // used in production to create signature
    // NOTE: api.secp256k1_sign() will perform an additional sha_256 hash operation on the given data
    #[cfg(target_arch = "wasm32")]
    let packet_signature = {
        deps.api
            .secp256k1_sign(&packet_hash, &signing_key_bytes)
            .map_err(|err| StdError::generic_err(err.to_string()))?
    };
    // let packet_signature = {
    //     PrivateKey::parse(&signing_key_bytes)?
    //         .sign(&packet_hash, deps.api)
    //         .serialize()
    //         .to_vec()
    // };

    // used only in unit testing to create signature
    #[cfg(not(target_arch = "wasm32"))]
    let packet_signature = {
        let secp = secp256k1::Secp256k1::signing_only();
        let sk = secp256k1::SecretKey::from_slice(&signing_key_bytes).unwrap();

        let packet_message = secp256k1::Message::from_slice(&sha_256(&packet_hash))
            .map_err(|err| StdError::generic_err(err.to_string()))?;

        secp.sign_ecdsa(&packet_message, &sk).serialize_compact()
    };

    // convert the hashes and signatures into hex byte strings
    // NOTE: we need to perform the additional sha_256 because that is what the secret network API method does
    // NOTE: we add an extra byte to the end of the signatures for `ecrecover` in Solidity
    // let task_id = format!("{:#04x}", &msg.task_id);
    let payload_hash = format!(
        "0x{}",
        task_info.payload_hash.as_slice().encode_hex::<String>()
    );
    let result = format!("0x{}", msg.result.encode_hex::<String>());
    let result_hash = format!("0x{}", sha_256(&result_hash).encode_hex::<String>());
    let result_signature = format!("0x{}{:x}", &result_signature.encode_hex::<String>(), 27);
    let packet_hash = format!("0x{}", sha_256(&packet_hash).encode_hex::<String>());
    let packet_signature = format!("0x{}{:x}", &packet_signature.encode_hex::<String>(), 27);

    Ok(Response::new()
        .add_attribute_plaintext("source_network", "secret")
        .add_attribute_plaintext("task_destination_network", routing_info)
        .add_attribute_plaintext("task_id", msg.task_id.to_string())
        .add_attribute_plaintext("payload_hash", payload_hash)
        .add_attribute_plaintext("result", result)
        .add_attribute_plaintext("result_hash", result_hash)
        .add_attribute_plaintext("result_signature", result_signature)
        .add_attribute_plaintext("packet_hash", packet_hash)
        .add_attribute_plaintext("packet_signature", packet_signature))
}

#[cfg(feature = "contract")]
/////////////////////////////////////// Query /////////////////////////////////////
/// Returns QueryResult
///
/// # Arguments
///
/// * `deps` - reference to Extern containing all the contract's external dependencies
/// * `msg` - QueryMsg passed in with the query call
#[entry_point]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    let response = match msg {
        QueryMsg::GetPublicKeys {} => query_public_keys(deps),
    };
    pad_query_result(response, BLOCK_SIZE)
}

// the encryption key will be a base64 string, the verifying key will be a '0x' prefixed hex string
fn query_public_keys(deps: Deps) -> StdResult<Binary> {
    let state: State = CONFIG.load(deps.storage)?;
    to_binary(&PublicKeyResponse {
        encryption_key: state.encryption_keys.pk,
        verification_key: format!(
            "0x{}",
            state.signing_keys.pk.as_slice().encode_hex::<String>()
        ),
    })
}

/////////////////////////////////////// Helpers /////////////////////////////////////

/// Returns (PublicKey, StaticSecret, Vec<u8>)
///
/// generates a public and privite key pair and generates a new PRNG_SEED with or without user entropy.
///
/// # Arguments
///
/// * `env` - contract's environment to be used for randomization
/// * `prng_seed` - required prng seed for randomization
/// * `user_entropy` - optional random string input by the user
pub fn generate_keypair(
    env: &Env,
    prng_seed: Vec<u8>,
    user_entropy: Option<String>,
) -> Result<(PrivateKey, PublicKey, Vec<u8>), StdError> {
    // generate new rng seed
    let new_prng_bytes: [u8; 32] = match user_entropy {
        Some(s) => new_entropy(env, prng_seed.as_ref(), s.as_bytes()),
        None => new_entropy(env, prng_seed.as_ref(), prng_seed.as_ref()),
    };

    // generate and return key pair
    let mut rng = Prng::new(prng_seed.as_ref(), new_prng_bytes.as_ref());
    let sk = PrivateKey::parse(&rng.rand_bytes())?;
    let pk = sk.pubkey();

    Ok((sk, pk, new_prng_bytes.to_vec()))
}

/// Returns [u8;32]
///
/// generates new entropy from block data, does not save it to the contract.
///
/// # Arguments
///
/// * `env` - Env of contract's environment
/// * `seed` - (user generated) seed for rng
/// * `entropy` - Entropy seed saved in the contract
pub fn new_entropy(env: &Env, seed: &[u8], entropy: &[u8]) -> [u8; 32] {
    // 16 here represents the lengths in bytes of the block height and time.
    let entropy_len = 16 + env.contract.address.to_string().len() + entropy.len();
    let mut rng_entropy = Vec::with_capacity(entropy_len);
    rng_entropy.extend_from_slice(&env.block.height.to_be_bytes());
    rng_entropy.extend_from_slice(&env.block.time.seconds().to_be_bytes());
    rng_entropy.extend_from_slice(env.contract.address.to_string().as_bytes());
    rng_entropy.extend_from_slice(entropy);

    let mut rng = Prng::new(seed, &rng_entropy);

    rng.rand_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use cosmwasm_std::testing::{mock_dependencies, mock_env, mock_info};
    use cosmwasm_std::{from_binary, Addr, Binary, Empty};

    use chacha20poly1305::aead::{Aead, NewAead};
    use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
    use secp256k1::{ecdh::SharedSecret, Message, Secp256k1, SecretKey};

    const OWNER: &str = "admin0001";
    const SOMEBODY: &str = "somebody";

    #[track_caller]
    fn setup_test_case(deps: DepsMut) -> Result<Response<Empty>, StdError> {
        // Instantiate a contract with entropy
        let admin = Some(Addr::unchecked(OWNER.to_owned()));
        let entropy = "secret".to_owned();
        let rng_hash = "string".to_string();
        let rng_addr = Addr::unchecked("address".to_string());

        let init_msg = InstantiateMsg {
            admin,
            entropy,
            rng_hash,
            rng_addr,
        };
        instantiate(deps, mock_env(), mock_info(OWNER, &[]), init_msg)
    }

    #[track_caller]
    fn get_gateway_encryption_key(deps: Deps) -> Binary {
        let query_msg = QueryMsg::GetPublicKeys {};
        let query_result = query(deps, mock_env(), query_msg);
        let query_answer: PublicKeyResponse = from_binary(&query_result.unwrap()).unwrap();
        let gateway_pubkey = query_answer.encryption_key;
        gateway_pubkey
    }

    #[track_caller]
    fn get_gateway_verification_key(deps: Deps) -> String {
        let query_msg = QueryMsg::GetPublicKeys {};
        let query_result = query(deps, mock_env(), query_msg);
        let query_answer: PublicKeyResponse = from_binary(&query_result.unwrap()).unwrap();
        let gateway_pubkey = query_answer.verification_key;
        gateway_pubkey
    }

    #[test]
    fn test_init() {
        let mut deps = mock_dependencies();

        let response = setup_test_case(deps.as_mut()).unwrap();
        assert_eq!(1, response.messages.len());
    }

    #[test]
    fn test_query() {
        let mut deps = mock_dependencies();
        let env = mock_env();
        let info = mock_info(OWNER, &[]);

        // initialize
        setup_test_case(deps.as_mut()).unwrap();

        // mock scrt-rng message
        let mut rng = Prng::new(&[1, 2, 3], &[4, 5, 6]);
        let fake_msg = ExecuteMsg::ReceiveFRn {
            cb_msg: Binary(vec![]),
            purpose: None,
            rn: rng.rand_bytes(),
        };
        execute(deps.as_mut(), env.clone(), info, fake_msg).unwrap();

        // query
        let msg = QueryMsg::GetPublicKeys {};
        let res = query(deps.as_ref(), env.clone(), msg);
        assert!(res.is_ok(), "query failed: {}", res.err().unwrap());
        let value: PublicKeyResponse = from_binary(&res.unwrap()).unwrap();
        assert_eq!(value.encryption_key.as_slice().len(), 33);
    }

    #[test]
    fn test_pre_execution() {
        let mut deps = mock_dependencies();
        let env = mock_env();
        let info = mock_info(OWNER, &[]);

        // initialize
        setup_test_case(deps.as_mut()).unwrap();

        // mock scrt-rng message
        let mut rng = Prng::new(&[1, 2, 3], &[4, 5, 6]);
        let fake_msg = ExecuteMsg::ReceiveFRn {
            cb_msg: Binary(vec![]),
            purpose: None,
            rn: rng.rand_bytes(),
        };
        execute(
            deps.as_mut(),
            env.clone(),
            mock_info("sender", &[]),
            fake_msg,
        )
        .unwrap();

        // get gateway public encryption key
        let gateway_pubkey = get_gateway_encryption_key(deps.as_ref());

        // mock key pair
        let secp = Secp256k1::new();
        let secret_key = Key::from_slice(b"an example very very secret key."); // 32-bytes
        let secret_key = SecretKey::from_slice(secret_key).unwrap();
        let public_key = secp256k1::PublicKey::from_secret_key(&secp, &secret_key);

        let wrong_secret_key = Key::from_slice(b"an example very wrong secret key"); // 32-bytes
        let wrong_secret_key = SecretKey::from_slice(wrong_secret_key).unwrap();
        let wrong_public_key = secp256k1::PublicKey::from_secret_key(&secp, &wrong_secret_key);

        // create shared key from user private + gateway public
        let gateway_pubkey = secp256k1::PublicKey::from_slice(gateway_pubkey.as_slice()).unwrap();
        let shared_key = SharedSecret::new(&gateway_pubkey, &secret_key);

        // mock Payload
        let data = "{\"fingerprint\": \"0xF9BA143B95FF6D82\", \"location\": \"Menlo Park, CA\"}"
            .to_string();
        let routing_info =
            Addr::unchecked("secret19zpyd046u4swqpksr3n44cej4j8pg6ahw95y85".to_string());
        let routing_code_hash =
            "2a2fbe493ef25b536bbe0baa3917b51e5ba092e14bd76abf50a59526e2789be3".to_string();
        let user_address = Addr::unchecked("some eth address".to_string());
        let user_key = Binary(public_key.serialize().to_vec());
        let user_pubkey = user_key.clone(); // TODO make this a unique key

        let payload = Payload {
            data: data.clone(),
            routing_info: routing_info.clone(),
            routing_code_hash: routing_code_hash.clone(),
            user_address: user_address.clone(),
            user_key: user_key.clone(),
        };
        let serialized_payload = to_binary(&payload).unwrap();

        // encrypt the payload
        let cipher = ChaCha20Poly1305::new_from_slice(shared_key.as_ref())
            .map_err(|_err| StdError::generic_err("could not create cipher".to_string()))
            .unwrap();
        let nonce = Nonce::from_slice(b"unique nonce"); // 12-bytes; unique per message
        let encrypted_payload = cipher
            .encrypt(nonce, serialized_payload.as_slice())
            .unwrap();

        // sign the payload
        let payload_hash = sha_256(serialized_payload.as_slice());
        let message = Message::from_slice(&payload_hash).unwrap();
        let payload_signature = secp.sign_ecdsa(&message, &secret_key);

        // mock wrong payload (encrypted with a key that does not match the one inside the payload)
        let wrong_user_address = Addr::unchecked("wrong eth address".to_string());
        let wrong_user_key = Binary(wrong_public_key.serialize().to_vec());

        let wrong_payload = Payload {
            data: data.clone(),
            routing_info: routing_info.clone(),
            routing_code_hash: routing_code_hash.clone(),
            user_address: wrong_user_address.clone(),
            user_key: wrong_user_key.clone(),
        };
        let wrong_serialized_payload = to_binary(&wrong_payload).unwrap();

        // encrypt the mock wrong payload
        let wrong_encrypted_payload = cipher
            .encrypt(nonce, wrong_serialized_payload.as_slice())
            .unwrap();

        // test payload user_key does not match given user_key
        let pre_execution_msg = PreExecutionMsg {
            task_id: 1,
            handle: "test".to_string(),
            routing_info: routing_info.clone(),
            routing_code_hash: routing_code_hash.clone(),
            user_address: user_address.clone(),
            user_key: user_key.clone(),
            user_pubkey: user_pubkey.clone(),
            payload: Binary(wrong_encrypted_payload.clone()),
            nonce: Binary(b"unique nonce".to_vec()),
            payload_hash: Binary(payload_hash.to_vec()),
            payload_signature: Binary(payload_signature.serialize_compact().to_vec()),
            source_network: "ethereum".to_string(),
        };
        let handle_msg = ExecuteMsg::Input {
            inputs: pre_execution_msg,
        };
        let err = execute(deps.as_mut(), env.clone(), info.clone(), handle_msg).unwrap_err();
        assert_eq!(err, StdError::generic_err("verification key mismatch"));

        // wrong routing info
        let wrong_routing_info =
            Addr::unchecked("secret13rcx3p8pxf0ttuvxk6czwu73sdccfz4w6e27fd".to_string());
        let routing_code_hash =
            "19438bf0cdf555c6472fb092eae52379c499681b36e47a2ef1c70f5269c8f02f".to_string();

        // test internal routing info does not match
        let pre_execution_msg = PreExecutionMsg {
            task_id: 1u64,
            source_network: "ethereum".to_string(),
            routing_info: wrong_routing_info.clone(),
            routing_code_hash: routing_code_hash.clone(),
            payload: Binary(encrypted_payload.clone()),
            payload_hash: Binary(payload_hash.to_vec()),
            payload_signature: Binary(payload_signature.serialize_compact().to_vec()),
            user_address: user_address.clone(),
            user_key: user_key.clone(),
            user_pubkey: user_pubkey.clone(),
            handle: "test".to_string(),
            nonce: Binary(b"unique nonce".to_vec()),
        };
        let handle_msg = ExecuteMsg::Input {
            inputs: pre_execution_msg,
        };
        let err = execute(deps.as_mut(), env.clone(), info.clone(), handle_msg).unwrap_err();
        assert_eq!(err, StdError::generic_err("routing info mismatch"));

        // test proper input handle
        let pre_execution_msg = PreExecutionMsg {
            task_id: 1u64,
            handle: "test".to_string(),
            routing_info,
            routing_code_hash,
            user_address,
            user_key,
            user_pubkey,
            payload: Binary(encrypted_payload),
            nonce: Binary(b"unique nonce".to_vec()),
            payload_hash: Binary(payload_hash.to_vec()),
            payload_signature: Binary(payload_signature.serialize_compact().to_vec()),
            source_network: "ethereum".to_string(),
        };
        let handle_msg = ExecuteMsg::Input {
            inputs: pre_execution_msg,
        };
        let handle_result = execute(deps.as_mut(), env.clone(), info, handle_msg);
        assert!(
            handle_result.is_ok(),
            "handle failed: {}",
            handle_result.err().unwrap()
        );
        let handle_answer: InputResponse =
            from_binary(&handle_result.unwrap().data.unwrap()).unwrap();
        assert_eq!(handle_answer.status, Success);
    }

    #[test]
    fn test_post_execution() {
        let mut deps = mock_dependencies();
        let env = mock_env();
        let info = mock_info(SOMEBODY, &[]);
        // initialize
        setup_test_case(deps.as_mut()).unwrap();

        // mock scrt-rng message
        let mut rng = Prng::new(&[1, 2, 3], &[4, 5, 6]);
        let fake_msg = ExecuteMsg::ReceiveFRn {
            cb_msg: Binary(vec![]),
            purpose: None,
            rn: rng.rand_bytes(),
        };
        execute(deps.as_mut(), env.clone(), info.clone(), fake_msg).unwrap();

        // get gateway public encryption key
        let gateway_pubkey = get_gateway_encryption_key(deps.as_ref());

        // mock key pair
        let secp = Secp256k1::new();
        let secret_key = Key::from_slice(b"an example very very secret key."); // 32-bytes
        let secret_key = SecretKey::from_slice(secret_key).unwrap();
        let public_key = secp256k1::PublicKey::from_secret_key(&secp, &secret_key);

        // create shared key from user private + gateway public
        let gateway_pubkey = secp256k1::PublicKey::from_slice(gateway_pubkey.as_slice()).unwrap();
        let shared_key = SharedSecret::new(&gateway_pubkey, &secret_key);

        // mock Payload
        let data = "{\"fingerprint\": \"0xF9BA143B95FF6D82\", \"location\": \"Menlo Park, CA\"}"
            .to_string();
        let routing_info =
            Addr::unchecked("secret19zpyd046u4swqpksr3n44cej4j8pg6ahw95y85".to_string());
        let routing_code_hash =
            "2a2fbe493ef25b536bbe0baa3917b51e5ba092e14bd76abf50a59526e2789be3".to_string();
        let user_address = Addr::unchecked("some eth address".to_string());
        let user_key = Binary(public_key.serialize().to_vec());
        let user_pubkey = user_key.clone(); // TODO make this a unique key

        let payload = Payload {
            data: data.clone(),
            routing_info: routing_info.clone(),
            routing_code_hash: routing_code_hash.clone(),
            user_address: user_address.clone(),
            user_key: user_key.clone(),
        };
        let serialized_payload = to_binary(&payload).unwrap();

        // encrypt the payload
        let cipher = ChaCha20Poly1305::new_from_slice(shared_key.as_ref())
            .map_err(|_err| StdError::generic_err("could not create cipher".to_string()))
            .unwrap();
        let nonce = Nonce::from_slice(b"unique nonce"); // 12-bytes; unique per message
        let encrypted_payload = cipher
            .encrypt(nonce, serialized_payload.as_slice())
            .expect("encryption failure!"); // NOTE: handle this error to avoid panics!

        // sign the payload
        let payload_hash = sha_256(serialized_payload.as_slice());
        let message = Message::from_slice(&payload_hash).unwrap();
        let payload_signature = secp.sign_ecdsa(&message, &secret_key);

        // execute input handle
        let pre_execution_msg = PreExecutionMsg {
            task_id: 1u64,
            source_network: "ethereum".to_string(),
            routing_info,
            routing_code_hash,
            payload: Binary(encrypted_payload),
            payload_hash: Binary(payload_hash.to_vec()),
            payload_signature: Binary(payload_signature.serialize_compact().to_vec()),
            user_address,
            user_key,
            user_pubkey: user_pubkey.clone(),
            handle: "test".to_string(),
            nonce: Binary(b"unique nonce".to_vec()),
        };
        let handle_msg = ExecuteMsg::Input {
            inputs: pre_execution_msg.clone(),
        };
        execute(deps.as_mut(), env.clone(), info.clone(), handle_msg).unwrap();

        // test incorrect input_hash
        let wrong_post_execution_msg = PostExecutionMsg {
            result: "{\"answer\": 42}".to_string(),
            task_id: 1u64,
            input_hash: Binary(sha_256("wrong data".as_bytes()).to_vec()),
        };
        let handle_msg = ExecuteMsg::Output {
            outputs: wrong_post_execution_msg,
        };
        let err = execute(deps.as_mut(), env.clone(), info.clone(), handle_msg).unwrap_err();
        assert_eq!(
            err,
            StdError::generic_err("input hash does not match task id")
        );

        // test output handle
        let post_execution_msg = PostExecutionMsg {
            result: "{\"answer\": 42}".to_string(),
            task_id: 1,
            input_hash: Binary(
                sha_256(&[data.as_bytes(), 1u64.to_le_bytes().as_ref()].concat()).to_vec(),
            ),
        };

        let handle_msg = ExecuteMsg::Output {
            outputs: post_execution_msg,
        };
        let handle_result = execute(deps.as_mut(), env.clone(), info.clone(), handle_msg);
        assert!(
            handle_result.is_ok(),
            "handle failed: {}",
            handle_result.err().unwrap()
        );
        let logs = handle_result.unwrap().attributes;

        let gateway_pubkey = get_gateway_verification_key(deps.as_ref());
        println!("Gateway public key: {:?}", gateway_pubkey);

        for log in logs.clone() {
            println!("{:?}, {:?}", log.key, log.value)
        }

        assert_eq!(logs[0].value, "secret".to_string());
        assert_eq!(logs[1].value, "ethereum".to_string());
        assert_eq!(logs[2].value, "1".to_string());
        assert_eq!(
            hex::decode(logs[3].value.clone().strip_prefix("0x").unwrap())
                .unwrap()
                .len(),
            32
        );
        assert_eq!(logs[4].value, "0x7b22616e73776572223a2034327d".to_string());

        assert_eq!(
            hex::decode(logs[5].value.clone().strip_prefix("0x").unwrap())
                .unwrap()
                .len(),
            32
        );
        assert_eq!(
            hex::decode(logs[6].value.clone().strip_prefix("0x").unwrap())
                .unwrap()
                .len(),
            65
        );
        assert_eq!(
            hex::decode(logs[7].value.clone().strip_prefix("0x").unwrap())
                .unwrap()
                .len(),
            32
        );
        assert_eq!(
            hex::decode(logs[8].value.clone().strip_prefix("0x").unwrap())
                .unwrap()
                .len(),
            65
        );
    }
}
