# Rust API review inventory

This inventory covers every active entry in the
[upstream Rust API checklist](https://rust-lang.github.io/api-guidelines/checklist.html).
There are **54** entries. Links below open the explanatory
sections.
Read those sections for the rule and its qualifications; the trigger column is
only a repository review prompt.

This inventory defines the review checklist for contributors. The table preserves
the initial pending state for a new component and does not claim compliance.
Assess changes against the exact revisions in
[standards-sources.json](standards-sources.json). Keep completed component
assessments local and reuse them for later changes; include relevant findings
and verification summaries in the change description.

The authority and embedded mappings are in [coding-standard.md](coding-standard.md).

## Review procedure

For a new crate, assess every row. For a later change, reuse the crate's record,
review the affected rows, and reconsider prior applicability decisions when its
public API, target, features, dependencies, or release status changes. Record
the crate/change identifier and supported build configurations with the review.

Use **applies** with implementation/check evidence, **not applicable** with the
specific absent construct or scope, **exception** with the narrow reason and
consequence, or **pending** when unresolved. Do not turn an inconvenient
applicable rule into “not applicable.” An exception needs the source, target,
alternatives, consequence, and revisit condition; the standard describes this
process. Keep working decisions in a local review record and document durable
contracts in crate documentation. Do not mutate this inventory into a claim about
all future code.

| Guideline and explanation | Applicability trigger in this repository | Initial state / evidence |
| --- | --- | --- |
| [C-CASE](https://rust-lang.github.io/api-guidelines/naming.html#c-case) | Authored Rust identifiers | Pending |
| [C-CONV](https://rust-lang.github.io/api-guidelines/naming.html#c-conv) | Conversion methods | Pending |
| [C-GETTER](https://rust-lang.github.io/api-guidelines/naming.html#c-getter) | Accessors | Pending |
| [C-ITER](https://rust-lang.github.io/api-guidelines/naming.html#c-iter) | Collection iteration | Pending |
| [C-ITER-TY](https://rust-lang.github.io/api-guidelines/naming.html#c-iter-ty) | Named iterator types | Pending |
| [C-FEATURE](https://rust-lang.github.io/api-guidelines/naming.html#c-feature) | Cargo feature definitions | Pending |
| [C-WORD-ORDER](https://rust-lang.github.io/api-guidelines/naming.html#c-word-order) | Related identifier families | Pending |
| [C-COMMON-TRAITS](https://rust-lang.github.io/api-guidelines/interoperability.html#c-common-traits) | Exposed types; review resource ownership | Pending |
| [C-CONV-TRAITS](https://rust-lang.github.io/api-guidelines/interoperability.html#c-conv-traits) | Conversions between types | Pending |
| [C-COLLECT](https://rust-lang.github.io/api-guidelines/interoperability.html#c-collect) | Collections; review capacity failures | Pending |
| [C-SERDE](https://rust-lang.github.io/api-guidelines/interoperability.html#c-serde) | Data structures, configuration, result records | Pending |
| [C-SEND-SYNC](https://rust-lang.github.io/api-guidelines/interoperability.html#c-send-sync) | Exposed types and execution contexts | Pending |
| [C-GOOD-ERR](https://rust-lang.github.io/api-guidelines/interoperability.html#c-good-err) | Public failures; review `no_std` mapping | Pending |
| [C-NUM-FMT](https://rust-lang.github.io/api-guidelines/interoperability.html#c-num-fmt) | Numeric/bit-oriented wrapper types | Pending |
| [C-RW-VALUE](https://rust-lang.github.io/api-guidelines/interoperability.html#c-rw-value) | Generic I/O interfaces; inspect actual traits | Pending |
| [C-EVOCATIVE](https://rust-lang.github.io/api-guidelines/macros.html#c-evocative) | Authored macro interfaces | Pending |
| [C-MACRO-ATTR](https://rust-lang.github.io/api-guidelines/macros.html#c-macro-attr) | Macros producing items | Pending |
| [C-ANYWHERE](https://rust-lang.github.io/api-guidelines/macros.html#c-anywhere) | Item macros and supported invocation scopes | Pending |
| [C-MACRO-VIS](https://rust-lang.github.io/api-guidelines/macros.html#c-macro-vis) | Macro-generated public/private items | Pending |
| [C-MACRO-TY](https://rust-lang.github.io/api-guidelines/macros.html#c-macro-ty) | Macro inputs containing type expressions | Pending |
| [C-CRATE-DOC](https://rust-lang.github.io/api-guidelines/documentation.html#c-crate-doc) | Library entry points | Pending |
| [C-EXAMPLE](https://rust-lang.github.io/api-guidelines/documentation.html#c-example) | Public APIs and relevant hardware examples | Pending |
| [C-QUESTION-MARK](https://rust-lang.github.io/api-guidelines/documentation.html#c-question-mark) | Examples with fallible operations | Pending |
| [C-FAILURE](https://rust-lang.github.io/api-guidelines/documentation.html#c-failure) | API errors, panics, unsafe preconditions | Pending |
| [C-LINK](https://rust-lang.github.io/api-guidelines/documentation.html#c-link) | Related API/target documentation | Pending |
| [C-METADATA](https://rust-lang.github.io/api-guidelines/documentation.html#c-metadata) | Package manifests and publication intent | Pending |
| [C-RELNOTES](https://rust-lang.github.io/api-guidelines/documentation.html#c-relnotes) | Versioned releases | Pending |
| [C-HIDDEN](https://rust-lang.github.io/api-guidelines/documentation.html#c-hidden) | Exported documentation surface | Pending |
| [C-SMART-PTR](https://rust-lang.github.io/api-guidelines/predictability.html#c-smart-ptr) | Authored smart pointers | Pending |
| [C-CONV-SPECIFIC](https://rust-lang.github.io/api-guidelines/predictability.html#c-conv-specific) | Placement of conversion APIs | Pending |
| [C-METHOD](https://rust-lang.github.io/api-guidelines/predictability.html#c-method) | Operations associated with a value | Pending |
| [C-NO-OUT](https://rust-lang.github.io/api-guidelines/predictability.html#c-no-out) | Return values and caller-owned buffers | Pending |
| [C-OVERLOAD](https://rust-lang.github.io/api-guidelines/predictability.html#c-overload) | Operator implementations | Pending |
| [C-DEREF](https://rust-lang.github.io/api-guidelines/predictability.html#c-deref) | `Deref`/`DerefMut` implementations | Pending |
| [C-CTOR](https://rust-lang.github.io/api-guidelines/predictability.html#c-ctor) | Initialization interfaces | Pending |
| [C-INTERMEDIATE](https://rust-lang.github.io/api-guidelines/flexibility.html#c-intermediate) | Reusable calculations and partial outcomes | Pending |
| [C-CALLER-CONTROL](https://rust-lang.github.io/api-guidelines/flexibility.html#c-caller-control) | Ownership and buffer APIs | Pending |
| [C-GENERIC](https://rust-lang.github.io/api-guidelines/flexibility.html#c-generic) | Reusable parameter contracts and code size | Pending |
| [C-OBJECT](https://rust-lang.github.io/api-guidelines/flexibility.html#c-object) | Traits intended for dynamic dispatch | Pending |
| [C-NEWTYPE](https://rust-lang.github.io/api-guidelines/type-safety.html#c-newtype) | Units, identifiers, modes, semantic wrappers | Pending |
| [C-CUSTOM-TYPE](https://rust-lang.github.io/api-guidelines/type-safety.html#c-custom-type) | Ambiguous parameters/configuration | Pending |
| [C-BITFLAG](https://rust-lang.github.io/api-guidelines/type-safety.html#c-bitflag) | Combinable flags | Pending |
| [C-BUILDER](https://rust-lang.github.io/api-guidelines/type-safety.html#c-builder) | Complex construction contracts | Pending |
| [C-VALIDATE](https://rust-lang.github.io/api-guidelines/dependability.html#c-validate) | Input boundaries and invariants | Pending |
| [C-DTOR-FAIL](https://rust-lang.github.io/api-guidelines/dependability.html#c-dtor-fail) | Resource cleanup | Pending |
| [C-DTOR-BLOCK](https://rust-lang.github.io/api-guidelines/dependability.html#c-dtor-block) | Cleanup with transfer/wait/shutdown behavior | Pending |
| [C-DEBUG](https://rust-lang.github.io/api-guidelines/debuggability.html#c-debug) | Public types; review peripheral side effects | Pending |
| [C-DEBUG-NONEMPTY](https://rust-lang.github.io/api-guidelines/debuggability.html#c-debug-nonempty) | Public debug representations | Pending |
| [C-SEALED](https://rust-lang.github.io/api-guidelines/future-proofing.html#c-sealed) | Deliberately closed trait implementation sets | Pending |
| [C-STRUCT-PRIVATE](https://rust-lang.github.io/api-guidelines/future-proofing.html#c-struct-private) | Public representations and mutation invariants | Pending |
| [C-NEWTYPE-HIDE](https://rust-lang.github.io/api-guidelines/future-proofing.html#c-newtype-hide) | Exposed implementation-dependent types | Pending |
| [C-STRUCT-BOUNDS](https://rust-lang.github.io/api-guidelines/future-proofing.html#c-struct-bounds) | Generic data structure definitions | Pending |
| [C-STABLE](https://rust-lang.github.io/api-guidelines/necessities.html#c-stable) | Stable public API commitments | Pending |
| [C-PERMISSIVE](https://rust-lang.github.io/api-guidelines/necessities.html#c-permissive) | Project/dependency licensing and distribution | Pending |
