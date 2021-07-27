use crate::cases::ratio_to_gas_signed;
use crate::testbed_runners::{end_count, start_count, GasMetric};
use crate::vm_estimator::{create_context, least_squares_method};
use near_primitives::config::VMConfig;
use near_primitives::contract::ContractCode;
use near_primitives::profile::ProfileData;
use near_primitives::runtime::fees::RuntimeFeesConfig;
use near_primitives::types::{CompiledContractCache, ProtocolVersion};
use near_store::{create_store, StoreCompiledContractCache};
use near_vm_logic::mocks::mock_external::MockedExternal;
use near_vm_runner::{run_vm, VMKind};
use nearcore::get_store_path;
use std::fmt::Write;
use std::sync::Arc;
use near_test_contracts::{aurora_contract, get_aurora_contract_data, get_multisig_contract_data};
use num_rational::Ratio;

const REPEATS: u64 = 50;

#[allow(dead_code)]
fn test_function_call(metric: GasMetric, vm_kind: VMKind) {
    let mut xs = vec![];
    let mut ys = vec![];
    for method_count in vec![5, 20, 30, 50, 100, 200, 1000] {
        let contract = make_many_methods_contract(method_count);
        println!("LEN = {}", contract.get_code().len());
        let cost = compute_function_call_cost(metric, vm_kind, REPEATS, &contract, "hello0");
        println!("{:?} {:?} {} {}", vm_kind, metric, method_count, cost / REPEATS);
        xs.push(contract.get_code().len() as u64);
        ys.push(cost / REPEATS);
    }

    // Regression analysis only makes sense for additive metrics.
    if metric == GasMetric::Time {
        return;
    }

    let (cost_base, cost_byte, _) = least_squares_method(&xs, &ys);

    println!(
        "{:?} {:?} function call base {} gas, per byte {} gas",
        vm_kind,
        metric,
        ratio_to_gas_signed(metric, cost_base),
        ratio_to_gas_signed(metric, cost_byte),
    );
}

#[test]
fn test_function_call_time() {
    // Run with
    // cargo test --release --lib function_call::test_function_call_time
    //    --features required  -- --exact --nocapture
    test_function_call(GasMetric::Time, VMKind::Wasmer0);
    test_function_call(GasMetric::Time, VMKind::Wasmer1);
    test_function_call(GasMetric::Time, VMKind::Wasmtime);
}

#[test]
fn test_function_call_icount() {
    // Use smth like
    // CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER=./runner.sh \
    // cargo test --release --features no_cpu_compatibility_checks,required  \
    // --lib function_call::test_function_call_icount -- --exact --nocapture
    // Where runner.sh is
    // /host/nearcore/runtime/runtime-params-estimator/emu-cost/counter_plugin/qemu-x86_64 \
    // -cpu Westmere-v1 -plugin file=/host/nearcore/runtime/runtime-params-estimator/emu-cost/counter_plugin/libcounter.so $@
    test_function_call(GasMetric::ICount, VMKind::Wasmer0);
    test_function_call(GasMetric::ICount, VMKind::Wasmer1);
    test_function_call(GasMetric::ICount, VMKind::Wasmtime);
}

#[test]
fn compare_function_call_icount() {
    // Base comparison
    test_function_call(GasMetric::ICount, VMKind::Wasmer0);

    let contracts_data = vec![get_aurora_contract_data(), get_multisig_contract_data()];
    for (contract, method_name) in contracts_data.iter().cloned() {
        // Actual cost
        let contract = ContractCode::new(contract.iter().cloned().collect(), None);
        let cost = compute_function_call_cost(GasMetric::ICount, VMKind::Wasmer0, REPEATS, &contract, method_name);
        let actual_gas = ratio_to_gas_signed(GasMetric::ICount, Ratio::new(cost as i128, REPEATS as i128));
        println!("actual = {}", actual_gas);

        // Old estimation
        let runtime_fees_config = RuntimeFeesConfig::default();
        let fee = runtime_fees_config.action_creation_config.function_call_cost.execution;
        // runtime_fees_config.action_creation_config.function_call_cost_per_byte is negligible here
        println!("old estimation = {}", fee);

        // New estimation
        // Prev computed:
        // let new_fee = 37_732_719_837 + 76_128_437 * contract.get_code().len();
        // Newly:
        // Wasmer0 ICount function call base 48080046101 gas, per byte 207939579 gas
        let new_fee = 48_080_046_101 + 207_939_579 * contract.get_code().len();
        println!("new estimation = {}", new_fee);

        println!("{},{},{},{}", method_name, actual_gas, fee, new_fee);
    }
}

fn make_many_methods_contract(method_count: i32) -> ContractCode {
    let mut methods = String::new();
    for i in 0..method_count {
        write!(
            &mut methods,
            "
            (export \"hello{}\" (func {i}))
              (func (;{i};)
                i32.const {i}
                drop
                return
              )
            ",
            i = i
        )
            .unwrap();
    }

    let code = format!(
        "
        (module
            {}
            )",
        methods
    );
    ContractCode::new(wat::parse_str(code).unwrap(), None)
}

pub fn compute_function_call_cost(
    gas_metric: GasMetric,
    vm_kind: VMKind,
    repeats: u64,
    contract: &ContractCode,
    method_name: &str,
) -> u64 {
    let workdir = tempfile::Builder::new().prefix("runtime_testbed").tempdir().unwrap();
    let store = create_store(&get_store_path(workdir.path()));
    let cache_store = Arc::new(StoreCompiledContractCache { store });
    let cache: Option<&dyn CompiledContractCache> = Some(cache_store.as_ref());
    let vm_config = VMConfig::default();
    let mut fake_external = MockedExternal::new();
    let fake_context = create_context(vec![]);
    let fees = RuntimeFeesConfig::default();
    let promise_results = vec![];

    // Warmup.
    if repeats != 1 {
        let result = run_vm(
            &contract,
            method_name,
            &mut fake_external,
            fake_context.clone(),
            &vm_config,
            &fees,
            &promise_results,
            vm_kind,
            ProtocolVersion::MAX,
            cache,
            ProfileData::new(),
        );
        assert!(result.1.is_none());
    }
    // Run with gas metering.
    let start = start_count(gas_metric);
    for _ in 0..repeats {
        let result = run_vm(
            &contract,
            method_name,
            &mut fake_external,
            fake_context.clone(),
            &vm_config,
            &fees,
            &promise_results,
            vm_kind,
            ProtocolVersion::MAX,
            cache,
            ProfileData::new(),
        );
        assert!(result.1.is_none());
    }
    let total_raw = end_count(gas_metric, &start) as i128;

    println!("cost is {}", total_raw);

    total_raw as u64
}