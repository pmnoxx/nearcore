use crate::errors::IntoVMError;
use crate::prepare;
use crate::wasmer1_runner::wasmer1_vm_hash;
use crate::wasmer_runner::wasmer0_vm_hash;
use crate::wasmtime_runner::wasmtime_vm_hash;
use borsh::{BorshDeserialize, BorshSerialize};
#[cfg(not(feature = "no_cache"))]
use cached::{cached_key, SizedCache};
use near_primitives::hash::CryptoHash;
use near_primitives::types::CompiledContractCache;
use near_vm_errors::CacheError::{DeserializationError, ReadError, SerializationError, WriteError};
use near_vm_errors::{CacheError, VMError};
use near_vm_logic::{VMConfig, VMKind};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::fmt;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, BorshDeserialize, BorshSerialize)]
enum ContractCacheKey {
    Version1 {
        code_hash: CryptoHash,
        vm_config_non_crypto_hash: u64,
        vm_kind: VMKind,
    },
    Version2 {
        code_hash: CryptoHash,
        vm_config_non_crypto_hash: u64,
        vm_kind: VMKind,
        vm_hash: u64,
    },
}

#[derive(Debug, Clone, BorshDeserialize, BorshSerialize)]
enum CacheRecord {
    Error(VMError),
    Code(Vec<u8>),
}

fn vm_hash(vm_kind: VMKind) -> u64 {
    match vm_kind {
        VMKind::Wasmer0 => wasmer0_vm_hash(),
        VMKind::Wasmer1 => wasmer1_vm_hash(),
        VMKind::Wasmtime => wasmtime_vm_hash(),
    }
}

fn get_key(code_hash: &[u8], code: &[u8], vm_kind: VMKind, config: &VMConfig) -> CryptoHash {
    let hash = match CryptoHash::try_from(code_hash) {
        Ok(hash) => hash,
        // Sometimes caller doesn't compute code_hash, so hash the code ourselves.
        Err(_e) => near_primitives::hash::hash(code),
    };
    let key = ContractCacheKey::Version2 {
        code_hash: hash,
        vm_config_non_crypto_hash: config.non_crypto_hash(),
        vm_kind,
        vm_hash: vm_hash(vm_kind),
    };
    near_primitives::hash::hash(&key.try_to_vec().unwrap())
}

fn cache_error(error: VMError, key: &CryptoHash, cache: &dyn CompiledContractCache) -> VMError {
    let record = CacheRecord::Error(error.clone());
    if cache.put(&(key.0).0, &record.try_to_vec().unwrap()).is_err() {
        VMError::CacheError(WriteError)
    } else {
        error
    }
}

#[derive(Default)]
pub struct MockCompiledContractCache {
    store: Arc<Mutex<HashMap<Vec<u8>, Vec<u8>>>>,
}

impl MockCompiledContractCache {
    pub fn len(&self) -> usize {
        self.store.lock().unwrap().len()
    }
}

impl CompiledContractCache for MockCompiledContractCache {
    fn put(&self, key: &[u8], value: &[u8]) -> Result<(), std::io::Error> {
        self.store.lock().unwrap().insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, std::io::Error> {
        let res = self.store.lock().unwrap().get(key).cloned();
        Ok(res)
    }
}

impl fmt::Debug for MockCompiledContractCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let guard = self.store.lock().unwrap();
        let hm: &HashMap<_, _> = &*guard;
        fmt::Debug::fmt(hm, f)
    }
}

#[cfg(feature = "wasmer0_vm")]
pub mod wasmer0_cache {
    use super::*;
    use wasmer_runtime::{compiler_for_backend, Backend};
    use wasmer_runtime_core::cache::Artifact;
    use wasmer_runtime_core::load_cache_with;

    pub(crate) fn compile_module(
        code: &[u8],
        config: &VMConfig,
    ) -> Result<wasmer_runtime::Module, VMError> {
        let prepared_code = prepare::prepare_contract(code, config)?;
        wasmer_runtime::compile(&prepared_code).map_err(|err| err.into_vm_error())
    }

    pub(crate) fn compile_and_serialize_wasmer(
        wasm_code: &[u8],
        config: &VMConfig,
        key: &CryptoHash,
        cache: &dyn CompiledContractCache,
    ) -> Result<wasmer_runtime::Module, VMError> {
        let _span = tracing::debug_span!("compile_and_serialize_wasmer");

        let module = compile_module(wasm_code, config).map_err(|e| cache_error(e, &key, cache))?;
        let artifact = module
            .cache()
            .map_err(|_e| VMError::CacheError(SerializationError { hash: (key.0).0 }))?;
        let code = artifact
            .serialize()
            .map_err(|_e| VMError::CacheError(SerializationError { hash: (key.0).0 }))?;
        let serialized = CacheRecord::Code(code).try_to_vec().unwrap();
        cache.put(key.as_ref(), &serialized).map_err(|_e| VMError::CacheError(WriteError))?;
        Ok(module)
    }

    /// Deserializes contract or error from the binary data. Signature means that we could either
    /// return module or cached error, which both considered to be `Ok()`, or encounter an error during
    /// the deserialization process.
    fn deserialize_wasmer(
        serialized: &[u8],
    ) -> Result<Result<wasmer_runtime::Module, VMError>, CacheError> {
        let _span = tracing::debug_span!("deserialize_wasmer");

        let record = CacheRecord::try_from_slice(serialized).map_err(|_e| DeserializationError)?;
        let serialized_artifact = match record {
            CacheRecord::Error(err) => return Ok(Err(err)),
            CacheRecord::Code(code) => code,
        };
        let artifact = Artifact::deserialize(serialized_artifact.as_slice())
            .map_err(|_e| CacheError::DeserializationError)?;
        unsafe {
            let compiler = compiler_for_backend(Backend::Singlepass).unwrap();
            match load_cache_with(artifact, compiler.as_ref()) {
                Ok(module) => Ok(Ok(module)),
                Err(_) => Err(CacheError::DeserializationError),
            }
        }
    }

    fn compile_module_cached_wasmer_impl(
        key: CryptoHash,
        wasm_code: &[u8],
        config: &VMConfig,
        cache: Option<&dyn CompiledContractCache>,
    ) -> Result<wasmer_runtime::Module, VMError> {
        if cache.is_none() {
            return compile_module(wasm_code, config);
        }

        let cache = cache.unwrap();
        match cache.get(&(key.0).0) {
            Ok(serialized) => match serialized {
                Some(serialized) => {
                    deserialize_wasmer(serialized.as_slice()).map_err(VMError::CacheError)?
                }
                None => compile_and_serialize_wasmer(wasm_code, config, &key, cache),
            },
            Err(_) => Err(VMError::CacheError(ReadError)),
        }
    }

    #[cfg(not(feature = "no_cache"))]
    const CACHE_SIZE: usize = 128;

    #[cfg(not(feature = "no_cache"))]
    cached_key! {
        MODULES: SizedCache<CryptoHash, Result<wasmer_runtime::Module, VMError>>
            = SizedCache::with_size(CACHE_SIZE);
        Key = {
            key
        };

        fn memcache_compile_module_cached_wasmer(
            key: CryptoHash,
            wasm_code: &[u8],
            config: &VMConfig,
            cache: Option<&dyn CompiledContractCache>) -> Result<wasmer_runtime::Module, VMError> = {
            compile_module_cached_wasmer_impl(key, wasm_code, config, cache)
        }
    }

    pub(crate) fn compile_module_cached_wasmer0(
        wasm_code_hash: &[u8],
        wasm_code: &[u8],
        config: &VMConfig,
        cache: Option<&dyn CompiledContractCache>,
    ) -> Result<wasmer_runtime::Module, VMError> {
        let key = get_key(wasm_code_hash, wasm_code, VMKind::Wasmer0, config);
        #[cfg(not(feature = "no_cache"))]
        return memcache_compile_module_cached_wasmer(key, wasm_code, config, cache);
        #[cfg(feature = "no_cache")]
        return compile_module_cached_wasmer_impl(key, wasm_code, config, cache);
    }
}

#[cfg(feature = "wasmer1_vm")]
pub mod wasmer1_cache {
    use super::*;
    pub(crate) fn compile_module_cached_wasmer1(
        wasm_code_hash: &[u8],
        wasm_code: &[u8],
        config: &VMConfig,
        cache: Option<&dyn CompiledContractCache>,
        store: &wasmer::Store,
    ) -> Result<wasmer::Module, VMError> {
        let key = get_key(wasm_code_hash, wasm_code, VMKind::Wasmer1, config);
        return compile_module_cached_wasmer1_impl(key, wasm_code, config, cache, store);
    }

    fn compile_module_wasmer1(
        code: &[u8],
        config: &VMConfig,
        store: &wasmer::Store,
    ) -> Result<wasmer::Module, VMError> {
        let prepared_code = prepare::prepare_contract(code, config)?;
        wasmer::Module::new(&store, prepared_code).map_err(|err| err.into_vm_error())
    }

    pub(crate) fn compile_and_serialize_wasmer1(
        wasm_code: &[u8],
        key: &CryptoHash,
        config: &VMConfig,
        cache: &dyn CompiledContractCache,
        store: &wasmer::Store,
    ) -> Result<wasmer::Module, VMError> {
        let _span = tracing::debug_span!("compile_and_serialize_wasmer1");

        let module = compile_module_wasmer1(wasm_code, config, store)
            .map_err(|e| cache_error(e, &key, cache))?;
        let code = module
            .serialize()
            .map_err(|_e| VMError::CacheError(SerializationError { hash: (key.0).0 }))?;
        let serialized = CacheRecord::Code(code).try_to_vec().unwrap();
        cache.put(key.as_ref(), &serialized).map_err(|_e| VMError::CacheError(WriteError))?;
        Ok(module)
    }

    fn deserialize_wasmer1(
        serialized: &[u8],
        store: &wasmer::Store,
    ) -> Result<Result<wasmer::Module, VMError>, CacheError> {
        let _span = tracing::debug_span!("deserialize_wasmer1");

        let record = CacheRecord::try_from_slice(serialized).map_err(|_e| DeserializationError)?;
        let serialized_module = match record {
            CacheRecord::Error(err) => return Ok(Err(err)),
            CacheRecord::Code(code) => code,
        };
        unsafe {
            Ok(Ok(wasmer::Module::deserialize(store, serialized_module.as_slice())
                .map_err(|_e| CacheError::DeserializationError)?))
        }
    }

    fn compile_module_cached_wasmer1_impl(
        key: CryptoHash,
        wasm_code: &[u8],
        config: &VMConfig,
        cache: Option<&dyn CompiledContractCache>,
        store: &wasmer::Store,
    ) -> Result<wasmer::Module, VMError> {
        if cache.is_none() {
            return compile_module_wasmer1(wasm_code, config, store);
        }

        let cache = cache.unwrap();
        match cache.get(&(key.0).0) {
            Ok(serialized) => match serialized {
                Some(serialized) => deserialize_wasmer1(serialized.as_slice(), store)
                    .map_err(VMError::CacheError)?,
                None => compile_and_serialize_wasmer1(wasm_code, &key, config, cache, store),
            },
            Err(_) => Err(VMError::CacheError(ReadError)),
        }
    }
}
