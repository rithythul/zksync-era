use std::sync::Arc;

use anyhow::Context;
use shivini::{gpu_proof_config::GpuProofConfig, gpu_prove_from_external_witness_data};
use zkevm_test_harness::{
    boojum::cs::implementations::setup::FinalizationHintsForProver,
    prover_utils::{verify_base_layer_proof, verify_recursion_layer_proof},
};
use zksync_prover_fri_types::{
    circuit_definitions::{
        base_layer_proof_config,
        boojum::{
            algebraic_props::{
                round_function::AbsorptionModeOverwrite, sponge::GoldilocksPoseidon2Sponge,
            },
            cs::implementations::{
                pow::NoPow, proof::Proof as CryptoProof, transcript::GoldilocksPoisedon2Transcript,
                witness::WitnessVec,
            },
            field::goldilocks::{GoldilocksExt2, GoldilocksField},
            worker::Worker,
        },
        circuit_definitions::{
            base_layer::{ZkSyncBaseLayerCircuit, ZkSyncBaseLayerProof},
            recursion_layer::{ZkSyncRecursionLayerProof, ZkSyncRecursiveLayerCircuit},
        },
        recursion_layer_proof_config,
    },
    FriProofWrapper,
};
#[cfg(feature = "gpu")]
use zksync_prover_keystore::GoldilocksGpuProverSetupData;

type Transcript = GoldilocksPoisedon2Transcript;
type Field = GoldilocksField;
type Hasher = GoldilocksPoseidon2Sponge<AbsorptionModeOverwrite>;
type Extension = GoldilocksExt2;
type Proof = CryptoProof<Field, Hasher, Extension>;

pub enum Circuit {
    Base(ZkSyncBaseLayerCircuit),
    Recursive(ZkSyncRecursiveLayerCircuit),
}

impl Circuit {
    #[cfg(feature = "gpu")]
    pub fn prove(
        &self,
        witness_vector: WitnessVec<GoldilocksField>,
        setup_data: Arc<GoldilocksGpuProverSetupData>,
    ) -> anyhow::Result<FriProofWrapper> {
        let worker = Worker::new();

        match self {
            Circuit::Base(circuit) => {
                let proof = Self::prove_base(circuit, witness_vector, setup_data, worker)?;
                let circuit_id = circuit.numeric_circuit_type();
                Ok(FriProofWrapper::Base(ZkSyncBaseLayerProof::from_inner(
                    circuit_id, proof,
                )))
            }
            Circuit::Recursive(circuit) => {
                let proof = Self::prove_recursive(circuit, witness_vector, setup_data, worker)?;
                let circuit_id = circuit.numeric_circuit_type();
                Ok(FriProofWrapper::Recursive(
                    ZkSyncRecursionLayerProof::from_inner(circuit_id, proof),
                ))
            }
        }
    }

    #[cfg(feature = "gpu")]
    fn prove_base(
        circuit: &ZkSyncBaseLayerCircuit,
        witness_vector: WitnessVec<GoldilocksField>,
        setup_data: Arc<GoldilocksGpuProverSetupData>,
        worker: Worker,
    ) -> anyhow::Result<Proof> {
        let gpu_proof_config = GpuProofConfig::from_base_layer_circuit(circuit);
        let boojum_proof_config = base_layer_proof_config();
        let proof = gpu_prove_from_external_witness_data::<Transcript, Hasher, NoPow, _>(
            &gpu_proof_config,
            &witness_vector,
            boojum_proof_config,
            &setup_data.setup,
            &setup_data.vk,
            (),
            &worker,
        )
        .context("failed to generate base proof")?
        .into();
        if !verify_base_layer_proof::<NoPow>(circuit, &proof, &setup_data.vk) {
            return Err(anyhow::anyhow!("failed to verify base proof"));
        }
        Ok(proof)
    }

    #[cfg(feature = "gpu")]
    fn prove_recursive(
        circuit: &ZkSyncRecursiveLayerCircuit,
        witness_vector: WitnessVec<GoldilocksField>,
        setup_data: Arc<GoldilocksGpuProverSetupData>,
        worker: Worker,
    ) -> anyhow::Result<Proof> {
        let gpu_proof_config = GpuProofConfig::from_recursive_layer_circuit(circuit);
        let boojum_proof_config = recursion_layer_proof_config();
        let proof = gpu_prove_from_external_witness_data::<Transcript, Hasher, NoPow, _>(
            &gpu_proof_config,
            &witness_vector,
            boojum_proof_config,
            &setup_data.setup,
            &setup_data.vk,
            (),
            &worker,
        )
        .context("failed to generate recursive proof")?
        .into();
        if !verify_recursion_layer_proof::<NoPow>(circuit, &proof, &setup_data.vk) {
            return Err(anyhow::anyhow!("failed to verify recursive proof"));
        }
        Ok(proof)
    }

    pub fn synthesize_vector(
        &self,
        finalization_hints: Arc<FinalizationHintsForProver>,
    ) -> anyhow::Result<WitnessVec<GoldilocksField>> {
        let cs = match self {
            Circuit::Base(circuit) => circuit.synthesis::<GoldilocksField>(&finalization_hints),
            Circuit::Recursive(circuit) => {
                circuit.synthesis::<GoldilocksField>(&finalization_hints)
            }
        };
        cs.witness
            .context("circuit is missing witness post synthesis")
    }
}
