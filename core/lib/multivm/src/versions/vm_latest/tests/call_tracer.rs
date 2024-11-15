use std::sync::Arc;

use once_cell::sync::OnceCell;
use zksync_types::{Address, Execute};

use super::TestedLatestVm;
use crate::{
    interface::{InspectExecutionMode, TxExecutionMode, VmInterface},
    tracers::CallTracer,
    utils::testonly::check_call_tracer_test_result,
    versions::testonly::{
        read_max_depth_contract, read_test_contract, ContractToDeploy, VmTesterBuilder,
    },
    vm_latest::{constants::BATCH_COMPUTATIONAL_GAS_LIMIT, ToTracerPointer},
};

// This test is ultra slow, so it's ignored by default.
#[test]
#[ignore]
fn test_max_depth() {
    let contarct = read_max_depth_contract();
    let address = Address::random();
    let mut vm = VmTesterBuilder::new()
        .with_empty_in_memory_storage()
        .with_rich_accounts(1)
        .with_bootloader_gas_limit(BATCH_COMPUTATIONAL_GAS_LIMIT)
        .with_execution_mode(TxExecutionMode::VerifyExecute)
        .with_custom_contracts(vec![ContractToDeploy::account(contarct, address)])
        .build::<TestedLatestVm>();

    let account = &mut vm.rich_accounts[0];
    let tx = account.get_l2_tx_for_execute(
        Execute {
            contract_address: Some(address),
            calldata: vec![],
            value: Default::default(),
            factory_deps: vec![],
        },
        None,
    );

    let result = Arc::new(OnceCell::new());
    let call_tracer = CallTracer::new(result.clone()).into_tracer_pointer();
    vm.vm.push_transaction(tx);
    let res = vm
        .vm
        .inspect(&mut call_tracer.into(), InspectExecutionMode::OneTx);
    assert!(result.get().is_some());
    assert!(res.result.is_failed());
}

#[test]
fn test_basic_behavior() {
    let contract = read_test_contract();
    let address = Address::repeat_byte(0xA5);
    let mut vm = VmTesterBuilder::new()
        .with_empty_in_memory_storage()
        .with_rich_accounts(1)
        .with_bootloader_gas_limit(BATCH_COMPUTATIONAL_GAS_LIMIT)
        .with_execution_mode(TxExecutionMode::VerifyExecute)
        .with_custom_contracts(vec![ContractToDeploy::account(contract, address)])
        .build::<TestedLatestVm>();

    let increment_by_6_calldata =
        "7cf5dab00000000000000000000000000000000000000000000000000000000000000006";

    let account = &mut vm.rich_accounts[0];
    let tx = account.get_l2_tx_for_execute(
        Execute {
            contract_address: Some(address),
            calldata: hex::decode(increment_by_6_calldata).unwrap(),
            value: Default::default(),
            factory_deps: vec![],
        },
        None,
    );

    let result = Arc::new(OnceCell::new());
    let call_tracer = CallTracer::new(result.clone()).into_tracer_pointer();
    vm.vm.push_transaction(tx);
    let res = vm
        .vm
        .inspect(&mut call_tracer.into(), InspectExecutionMode::OneTx);

    let call_tracer_result = result.get().unwrap();

    check_call_tracer_test_result(call_tracer_result);
    assert!(!res.result.is_failed());
}
