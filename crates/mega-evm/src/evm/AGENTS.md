# AGENTS.md

## OVERVIEW
MegaEVM execution core that wraps revm/op-revm with MegaETH instruction tables, host behavior, and execution interfaces.

## STRUCTURE
- `mod.rs`: `MegaEvm` wrapper, inspector toggling, execution convenience APIs.
- `context.rs`: execution context composition and state wiring.
- `execution.rs`: transaction execution flow and result shaping.
- `factory.rs`: `MegaEvmFactory` builder for context and external env wiring.
- `instructions.rs`: spec-layered opcode table and extension wrappers.
- `host.rs`: host overrides for volatile tracking, oracle reads, SALT gas hooks.
- `limit.rs`: EVM-facing limit helpers and runtime-limit adaptation.
- `spec.rs`: `MegaSpecId` parsing/ordering utilities.

## KEY PATTERNS
- Instruction semantics are layered wrappers, not ad-hoc per-opcode mutations.
- Preserve wrapper ordering when changing opcode stacks.
- Spec upgrades extend prior tables by overriding changed opcodes only.
- Host methods mark volatile accesses before delegating to inner host behavior.
- Oracle `sload` handling forces cold semantics for deterministic replay.
- `MegaEvm` methods read aggregate resource usage from `additional_limit` after execution.
- Keep inspector and non-inspector paths behaviorally aligned.

## Keyless sandbox hook
One hook into nested keyless-deploy sandbox execution: `SandboxInspector`, attached on `MegaContext` via `set_keyless_sandbox_hook` (forwarded from `MegaEvm` / `MegaBlockExecutor`) and detached via `clear_keyless_sandbox_hook`; setting a hook replaces the previous one.
Hook signatures match revm's `Inspector` on the sandbox EVM: `&mut` inputs and override return values are forwarded, so interventions take effect inside the sandbox as they would on a top-level EVM, and a hook that returns `None` from `call`/`create` and leaves its `&mut` inputs, outcomes, interpreter, and context unchanged observes without intervening (the same read-only notion the outer EVM's inspector has).
The slot is one type-erased handle (`Rc<RefCell<dyn SandboxInspector<E>>>`, held twice: parent env type and `EmptyExternalEnv`), and a sandbox's lifecycle events (`sandbox_start` / `sandbox_end`) go to the slot that also receives its opcode-level hooks: `EmptyExternalEnv` pre-REX4, the parent env from REX4 on.
Types generic over revm's `Inspector` get the hook through a blanket impl; hosts behind a generic EVM projection (a node's `ConfigureEvm::Evm`) name the attach operation on their own configuration type instead of on mega-evm.
With the `inspectors` feature, `sandbox::trace` ships the shared `revm-inspectors` pattern: a `SharedTracingInspector` for the outer EVM paired with a `SandboxTracer` attached as the hook, which records each sandbox execution in an arena of its own keyed by the intercepted call, and `splice_sandbox_traces`, which grafts those arenas under their `KeylessDeploy` CALL frames after execution and leaves the tracer empty; `mega-evme` and node tracing RPCs use the same implementation.

Contract:
1. With no hook attached, the sandbox path is unchanged.
2. Attaching a hook without intervening leaves result, state, gas, and usage identical to the unattached path.
3. Interventions take effect inside the sandbox as they would on a top-level EVM; reported `gas_used` and usage are the post-intervention values, and the parent frame records them as-is without a conservation check.
   Malformed synthetic outcomes, such as a `memory_offset` outside the frame's memory, panic exactly as they would on a top-level EVM; the sandbox neither isolates nor amplifies that.
4. The hook is node-local and non-consensus; an intervening node may diverge from the network, and the caller accepts that risk.
5. Later specs measure interventions and refuse some shapes; integrators must not depend on this base being permissive.

## WHERE TO LOOK
- New spec opcode delta: `instructions.rs` (`mini_rex`, `rex`, `rex2`, `rex3`, `rex4`, `rex5`, `rex6`, `rex7` tables; `rex6` and `rex7` currently alias their predecessor, expressing their deltas as `is_enabled` dispatch inside the shared handlers).
- Volatile access detention trigger changes: `host.rs` and volatile wrappers in `instructions.rs`.
- Call forwarding and stipend interplay: `instructions.rs` + `../limit/storage_call_stipend.rs`.
- New external gas pricing path: `host.rs` gas helper methods.
- Execution result fields exposed to callers: `execution.rs` and `mod.rs::execute_transaction`.
