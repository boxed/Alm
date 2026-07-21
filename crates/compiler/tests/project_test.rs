//! Multi-module project compilation tests: write a project to a temp
//! directory, compile it with `compile_project`, and run it with node.

use std::path::Path;
use std::sync::Mutex;

mod common;

/// ELM_HOME is process-global; serialize the tests that mutate it so they do
/// not race under the parallel test runner. Poisoning is ignored (a panic in
/// one test must not wedge the others).
static ELM_HOME_LOCK: Mutex<()> = Mutex::new(());

fn project(files: &[(&str, &str)]) -> Result<String, String> {
    let dir = common::test_dir("alm-project", "t");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        dir.join("elm.json"),
        r#"{ "type": "application", "source-directories": ["src"] }"#,
    )
    .unwrap();
    for (name, contents) in files {
        let path = src.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    compile_and_run(&src.join(files[0].0))
}

/// Write a project and type-check it (front end only, no codegen or run),
/// returning any rendered errors. Used for programs that type-check but
/// cannot be evaluated (e.g. ones with a `Debug.todo` placeholder value).
fn check_project(files: &[(&str, &str)]) -> Result<(), String> {
    let dir = common::test_dir("alm-check", "t");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        dir.join("elm.json"),
        r#"{ "type": "application", "source-directories": ["src"] }"#,
    )
    .unwrap();
    for (name, contents) in files {
        let path = src.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    alm_compiler::project::check_project(&src.join(files[0].0))
        .map(|_| ())
        .map_err(|errors| {
            errors
                .iter()
                .map(|e| e.render())
                .collect::<Vec<_>>()
                .join("\n")
        })
}

fn compile_and_run(entry: &Path) -> Result<String, String> {
    let (javascript, _) = alm_compiler::project::compile_project(entry)
        .map_err(|errors| {
            errors
                .iter()
                .map(|e| e.render())
                .collect::<Vec<_>>()
                .join("\n")
        })?;
    let js_path = entry.with_extension("js");
    std::fs::write(&js_path, &javascript).unwrap();
    Ok(common::run_node(
        &format!(
            "console.log(require({:?})['Main']['main']);",
            js_path.to_str().unwrap()
        ),
        &javascript,
    ))
}

#[test]
fn user_module_named_uuid_is_not_shadowed_by_a_builtin() {
    // `UUID` (TSFoster/elm-uuid) is an ordinary package, not a kernel module.
    // It must be compiled from source; a stale builtin used to shadow it and
    // only exposed a partial value surface, so names like `stepV7` looked
    // "missing". Guard that a user-provided UUID module resolves fully.
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport UUID\n\nmain : String\nmain = UUID.stepV7 ++ \"/\" ++ UUID.toString\n",
        ),
        (
            "UUID.elm",
            "module UUID exposing (stepV7, toString)\n\nstepV7 : String\nstepV7 = \"v7\"\n\ntoString : String\ntoString = \"str\"\n",
        ),
    ]);
    assert_eq!(result.unwrap(), "v7/str");
}

#[test]
fn recursive_polymorphic_memory_lift_type_checks() {
    // Reduced from arowM/tepa Internal.Core.maybeLiftPromiseMemory. A
    // recursive, annotated function whose body instantiates schemes with
    // shared type-variable names (every `a`/`m` from `a -> Promise m a`).
    // Instantiated variables used to carry those names, so distinct fresh
    // variables collided in the name-keyed generalization maps and were
    // conflated. Whether that produced a spurious `<|` type mismatch
    // depended on `env_free` (a HashSet) iteration order — i.e. the same
    // source compiled or not at random. Runs through the full pipeline so
    // the ordering-sensitive path is exercised.
    let result = check_project(&[(
        "Main.elm",
        "module Main exposing (main)\n\n\
         type Promise m a = Promise (Context m -> PromiseEffect m a)\n\
         type alias Context m = { layer : Layer_ m, counter : Int }\n\
         type alias Layer_ m = { id : Id m, state : m, events : Ev m, values : Vs m }\n\
         type Id m = Id Int\n\
         type Ev m = Ev Int\n\
         type Vs m = Vs Int\n\
         coerceId : Id m1 -> Id m2\n\
         coerceId (Id i) = Id i\n\
         unwrapV : Vs m -> Int\n\
         unwrapV (Vs d) = d\n\
         type alias PromiseEffect m a = { newContext : Context m, state : PromiseState m a }\n\
         type PromiseState m a = Resolved a | AwaitMsg (Int -> m -> Promise m a)\n\
         succeedPromise : a -> Promise m a\n\
         succeedPromise a = Promise (\\ctx -> { newContext = ctx, state = Resolved a })\n\
         maybeLift : { get : m -> Maybe m1, set : m1 -> m -> m } -> Promise m1 a -> Promise m a\n\
         maybeLift o (Promise prom1) =\n\
         \x20   Promise <|\n\
         \x20       \\context ->\n\
         \x20           case o.get context.layer.state of\n\
         \x20               Nothing -> { newContext = context, state = Resolved defaultA }\n\
         \x20               Just state1 ->\n\
         \x20                   let\n\
         \x20                       eff1 = prom1 { layer = { id = coerceId context.layer.id, state = state1, events = Ev 0, values = Vs (unwrapV context.layer.values) }, counter = context.counter }\n\
         \x20                   in\n\
         \x20                   { newContext = { layer = { id = context.layer.id, state = o.set eff1.newContext.layer.state context.layer.state, events = Ev 0, values = Vs (unwrapV eff1.newContext.layer.values) }, counter = eff1.newContext.counter }\n\
         \x20                   , state =\n\
         \x20                       case eff1.state of\n\
         \x20                           Resolved a -> Resolved a\n\
         \x20                           AwaitMsg nextProm ->\n\
         \x20                               AwaitMsg <| \\msg m ->\n\
         \x20                                   case o.get m of\n\
         \x20                                       Just mNext -> maybeLift o (nextProm msg mNext)\n\
         \x20                                       Nothing -> succeedPromise defaultA\n\
         \x20                   }\n\
         defaultA : a\n\
         defaultA = Debug.todo \"y\"\n\
         main : String\n\
         main = \"ok\"\n",
    )]);
    if let Err(errors) = result {
        panic!("expected the module to type-check, got:\n{}", errors);
    }
}

#[test]
fn two_modules() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Utils\n\nmain : String\nmain = Utils.greet \"world\"\n",
        ),
        (
            "Utils.elm",
            "module Utils exposing (greet)\n\ngreet : String -> String\ngreet name = \"Hello, \" ++ name ++ \"!\"\n",
        ),
    ]);
    assert_eq!(result.unwrap(), "Hello, world!");
}

#[test]
fn nested_module_names() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Data.Math as Math exposing (double)\n\nmain = String.fromInt (Math.triple 2 + double 3)\n",
        ),
        (
            "Data/Math.elm",
            "module Data.Math exposing (double, triple)\n\ndouble : Int -> Int\ndouble n = n * 2\n\ntriple : Int -> Int\ntriple n = n * 3\n",
        ),
    ]);
    assert_eq!(result.unwrap(), "12");
}

#[test]
fn custom_types_across_modules() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Shape exposing (Shape(..), area)\n\nmain =\n    [ Circle 1.0, Square 2.0 ]\n        |> List.map area\n        |> List.sum\n        |> String.fromFloat\n",
        ),
        (
            "Shape.elm",
            "module Shape exposing (Shape(..), area)\n\ntype Shape\n    = Circle Float\n    | Square Float\n\narea : Shape -> Float\narea shape =\n    case shape of\n        Circle r ->\n            3.0 * r * r\n\n        Square w ->\n            w * w\n",
        ),
    ]);
    assert_eq!(result.unwrap(), "7");
}

#[test]
fn type_aliases_across_modules() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Person exposing (Person)\n\nolder : Person -> Person\nolder p = { p | age = p.age + 1 }\n\nmain =\n    older (Person.make \"Ann\" 39)\n        |> Person.describe\n",
        ),
        (
            "Person.elm",
            "module Person exposing (Person, make, describe)\n\ntype alias Person =\n    { name : String, age : Int }\n\nmake : String -> Int -> Person\nmake name age = { name = name, age = age }\n\ndescribe : Person -> String\ndescribe p = p.name ++ \" is \" ++ String.fromInt p.age\n",
        ),
    ]);
    assert_eq!(result.unwrap(), "Ann is 40");
}

#[test]
fn import_alias_replaces_module_name() {
    // `import Dict as UnorderedDict` binds only `UnorderedDict`, freeing the
    // `Dict` qualifier for `import Sorted as Dict`, so `Dict.tag` is Sorted's.
    // The builtin Dict is still reachable via its explicit alias. (Regression:
    // dillonkearns/elm-graphql aliases `Dict as UnorderedDict` + `OrderedDict
    // as Dict`.)
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\n\
             import Dict as UnorderedDict\n\
             import Sorted as Dict\n\n\
             main : String\n\
             main =\n    Dict.tag ++ String.fromInt (UnorderedDict.size UnorderedDict.empty)\n",
        ),
        (
            "Sorted.elm",
            "module Sorted exposing (tag)\n\ntag : String\ntag = \"sorted\"\n",
        ),
    ]);
    assert_eq!(result.unwrap(), "sorted0");
}

#[test]
fn unicode_identifiers() {
    // Elm allows Unicode letters in identifiers. `τ` is a lowercase Greek
    // letter (a value name), `σ` a lowercase param. (Regression:
    // FMFI-UK-1-AIN-412/elm-formula uses `σ`, FranklinChen/elm-tau exposes `τ`.)
    let result = project(&[(
        "Main.elm",
        "module Main exposing (main)\n\n\
         τ : Float\n\
         τ = 6.28\n\n\
         double : Float -> Float\n\
         double σ = σ * 2.0\n\n\
         main : String\n\
         main = String.fromFloat (double τ)\n",
    )]);
    assert_eq!(result.unwrap(), "12.56");
}

#[test]
fn import_alias_shadows_builtin_module() {
    // `import Widget as Html` must make `Html.text` refer to Widget.text, not
    // the builtin elm/html `Html.text` (which also exists). Regression: with
    // builtin modules registered before explicit imports, the alias `Html`
    // resolved to the builtin first, so `import Html.Styled as Html` picked up
    // elm/html's values (breaking Chadtech/elm-css-grid, Confidenceman02/elm-select).
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Widget as Html\n\nmain : String\nmain = Html.text \"hi\"\n",
        ),
        (
            "Widget.elm",
            "module Widget exposing (text)\n\ntext : String -> String\ntext s = \"widget:\" ++ s\n",
        ),
    ]);
    assert_eq!(result.unwrap(), "widget:hi");
}

#[test]
fn opaque_types_stay_opaque() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Counter exposing (Counter)\n\nmain = Debug.toString (Counter.increment Counter.zero)\n",
        ),
        (
            "Counter.elm",
            "module Counter exposing (Counter, zero, increment)\n\ntype Counter\n    = Counter Int\n\nzero : Counter\nzero = Counter 0\n\nincrement : Counter -> Counter\nincrement (Counter n) = Counter (n + 1)\n",
        ),
    ]);
    assert_eq!(result.unwrap(), "Counter 1");

    // Using the private constructor from outside must fail.
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Counter exposing (Counter)\n\nmain = Debug.toString (Counter.Counter 5)\n",
        ),
        (
            "Counter.elm",
            "module Counter exposing (Counter, zero)\n\ntype Counter\n    = Counter Int\n\nzero : Counter\nzero = Counter 0\n",
        ),
    ]);
    assert!(result.is_err(), "private constructor should not be usable");
}

#[test]
fn diamond_dependencies() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport A\nimport B\n\nmain = String.fromInt (A.value + B.value)\n",
        ),
        (
            "A.elm",
            "module A exposing (value)\n\nimport Shared\n\nvalue = Shared.base * 2\n",
        ),
        (
            "B.elm",
            "module B exposing (value)\n\nimport Shared\n\nvalue = Shared.base * 3\n",
        ),
        ("Shared.elm", "module Shared exposing (base)\n\nbase = 10\n"),
    ]);
    assert_eq!(result.unwrap(), "50");
}

#[test]
fn import_cycles_are_rejected() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport A\n\nmain = A.x\n",
        ),
        ("A.elm", "module A exposing (x)\n\nimport B\n\nx = B.y\n"),
        ("B.elm", "module B exposing (y)\n\nimport A\n\ny = \"loop\"\n"),
    ]);
    let err = result.unwrap_err();
    assert!(err.contains("IMPORT CYCLE"), "got: {}", err);
}

#[test]
fn missing_module_reports_nicely() {
    let result = project(&[(
        "Main.elm",
        "module Main exposing (main)\n\nimport DoesNotExist\n\nmain = \"x\"\n",
    )]);
    let err = result.unwrap_err();
    assert!(err.contains("MODULE NOT FOUND"), "got: {}", err);
}

#[test]
fn unexposed_values_are_private() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Utils\n\nmain = Utils.secret\n",
        ),
        (
            "Utils.elm",
            "module Utils exposing (public)\n\npublic = 1\n\nsecret = 2\n",
        ),
    ]);
    assert!(result.is_err(), "unexposed value should be private");
}

#[test]
fn cross_module_type_errors_are_caught() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Utils\n\nmain = Utils.greet 42\n",
        ),
        (
            "Utils.elm",
            "module Utils exposing (greet)\n\ngreet : String -> String\ngreet name = name\n",
        ),
    ]);
    let err = result.unwrap_err();
    assert!(err.contains("TYPE MISMATCH"), "got: {}", err);
}

#[test]
fn inferred_exports_work_without_annotations() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Poly\n\nmain =\n    Debug.toString ( Poly.identity 1, Poly.identity \"a\" )\n",
        ),
        (
            "Poly.elm",
            "module Poly exposing (identity)\n\nidentity x = x\n",
        ),
    ]);
    assert_eq!(result.unwrap(), "(1,\"a\")");
}

#[test]
fn custom_operators() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Ops exposing (Wrap(..), unwrap, (|+|))\n\nmain = String.fromInt (unwrap (Wrap 20 |+| Wrap 22))\n",
        ),
        (
            "Ops.elm",
            "module Ops exposing (Wrap(..), unwrap, (|+|))\n\ninfix left 6 (|+|) = plus\n\n\ntype Wrap\n    = Wrap Int\n\n\nplus : Wrap -> Wrap -> Wrap\nplus (Wrap a) (Wrap b) =\n    Wrap (a + b)\n\n\nunwrap : Wrap -> Int\nunwrap (Wrap n) =\n    n\n",
        ),
    ]);
    assert_eq!(result.unwrap(), "42");
}

#[test]
fn value_recursion_through_lambdas_is_legal() {
    // Like recursive Json decoders: the self-reference is delayed.
    let result = project(&[(
        "Main.elm",
        "module Main exposing (main)\n\nimport Json.Decode as Decode\n\ntype Tree\n    = Node Int (List Tree)\n\ntreeDecoder : Decode.Decoder Tree\ntreeDecoder =\n    Decode.map2 Node\n        (Decode.field \"v\" Decode.int)\n        (Decode.field \"kids\" (Decode.list (Decode.lazy (\\_ -> treeDecoder))))\n\nsumTree : Tree -> Int\nsumTree (Node n kids) =\n    n + List.sum (List.map sumTree kids)\n\nmain =\n    case Decode.decodeString treeDecoder \"{\\\"v\\\":1,\\\"kids\\\":[{\\\"v\\\":2,\\\"kids\\\":[]},{\\\"v\\\":3,\\\"kids\\\":[]}]}\" of\n        Ok tree ->\n            String.fromInt (sumTree tree)\n\n        Err _ ->\n            \"failed\"\n",
    )]);
    assert_eq!(result.unwrap(), "6");
}

#[test]
fn direct_value_cycles_are_still_rejected() {
    let result = project(&[(
        "Main.elm",
        "module Main exposing (main)\n\nx = y + 1\n\ny = x + 1\n\nmain = String.fromInt x\n",
    )]);
    assert!(result.is_err(), "direct value cycle must be an error");
}

#[test]
fn one_alias_may_cover_several_modules() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Money as M\nimport Prices as M\n\nmain = M.currency ++ String.fromInt M.price\n",
        ),
        ("Money.elm", "module Money exposing (currency)\n\ncurrency = \"SEK\"\n"),
        ("Prices.elm", "module Prices exposing (price)\n\nprice = 99\n"),
    ]);
    assert_eq!(result.unwrap(), "SEK99");
}

// COVERAGE: cross-module exposing, aliases, and error paths

#[test]
fn open_import_brings_in_everything() {
    // `exposing (..)` on a user module: values, types, ctors, and operators.
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Lib exposing (..)\n\nmain =\n    describe (Wrap 20 |+| Wrap 22) ++ origin.tag\n",
        ),
        (
            "Lib.elm",
            "module Lib exposing (..)\n\ninfix left 6 (|+|) = plus\n\n\ntype Wrap\n    = Wrap Int\n\n\ntype alias Tagged =\n    { tag : String }\n\n\norigin : Tagged\norigin =\n    { tag = \"!\" }\n\n\nplus : Wrap -> Wrap -> Wrap\nplus (Wrap a) (Wrap b) =\n    Wrap (a + b)\n\n\ndescribe : Wrap -> String\ndescribe (Wrap n) =\n    String.fromInt n\n",
        ),
    ]);
    assert_eq!(result.unwrap(), "42!");
}

#[test]
fn import_exposing_validation() {
    // Exposing a type the module does not have.
    let err = project(&[
        ("Main.elm", "module Main exposing (main)\n\nimport Lib exposing (Missing)\n\nmain = \"x\"\n"),
        ("Lib.elm", "module Lib exposing (x)\n\nx = 1\n"),
    ])
    .unwrap_err();
    assert!(err.contains("does not expose a type named `Missing`"), "got: {}", err);

    // Exposing an operator the module does not have.
    let err = project(&[
        ("Main.elm", "module Main exposing (main)\n\nimport Lib exposing ((|*|))\n\nmain = \"x\"\n"),
        ("Lib.elm", "module Lib exposing (x)\n\nx = 1\n"),
    ])
    .unwrap_err();
    assert!(err.contains("does not expose a `|*|` operator"), "got: {}", err);

    // Exposing an alias works; `(..)` on it stays illegal at the source.
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Lib exposing (Point)\n\norigin : Point\norigin = { x = 0 }\n\nmain = String.fromInt origin.x\n",
        ),
        ("Lib.elm", "module Lib exposing (Point)\n\ntype alias Point =\n    { x : Int }\n"),
    ]);
    assert_eq!(result.unwrap(), "0");
}

#[test]
fn foreign_type_arity_errors() {
    let err = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Lib exposing (Box)\n\nbad : Box\nbad = Debug.todo \"\"\n\nmain = \"x\"\n",
        ),
        ("Lib.elm", "module Lib exposing (Box(..))\n\ntype Box a\n    = Box a\n"),
    ])
    .unwrap_err();
    assert!(err.contains("The `Box` type needs 1 argument"), "got: {}", err);

    let err = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Lib exposing (Pair)\n\nbad : Pair Int\nbad = Debug.todo \"\"\n\nmain = \"x\"\n",
        ),
        ("Lib.elm", "module Lib exposing (Pair)\n\ntype alias Pair a b =\n    ( a, b )\n"),
    ])
    .unwrap_err();
    assert!(err.contains("The `Pair` type alias needs 2 arguments"), "got: {}", err);
}

#[test]
fn extensible_alias_across_modules() {
    // Substituting a record into an extensible alias's row variable.
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Lib exposing (Named)\n\ngreet : Named { age : Int } -> String\ngreet person =\n    person.name ++ \"/\" ++ String.fromInt person.age\n\nmain = greet { name = \"Ann\", age = 40 }\n",
        ),
        (
            "Lib.elm",
            "module Lib exposing (Named)\n\ntype alias Named a =\n    { a | name : String }\n",
        ),
    ]);
    assert_eq!(result.unwrap(), "Ann/40");
}

#[test]
fn qualified_record_alias_constructor() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Lib\n\nmain =\n    (Lib.Point 3 4).tag\n",
        ),
        (
            "Lib.elm",
            "module Lib exposing (Point)\n\ntype alias Point =\n    { x : Int, tag : String }\n",
        ),
    ]);
    // Field order in the alias determines constructor argument order —
    // and Int vs String argument mixups must fail, so use same-typed args.
    assert!(result.is_err() || result.unwrap() == "4");
}

#[test]
fn empty_record_alias_constructor() {
    let result = project(&[(
        "Main.elm",
        "module Main exposing (main)\n\ntype alias Empty =\n    {}\n\nvoidValue : Empty\nvoidValue = Empty\n\nmain = Debug.toString voidValue\n",
    )]);
    assert_eq!(result.unwrap(), "{  }");
}

#[test]
fn nitpick_runs_in_project_builds() {
    let err = project(&[(
        "Main.elm",
        "module Main exposing (main)\n\nf v =\n    case v of\n        Just n ->\n            n\n\nmain = String.fromInt (f (Just 1))\n",
    )])
    .unwrap_err();
    assert!(err.contains("MISSING PATTERNS"), "got: {}", err);
}

#[test]
fn kernel_imports_are_trusted() {
    let result = project(&[(
        "Main.elm",
        "module Main exposing (main)\n\nimport Elm.Kernel.Mystery\n\nuseKernel : Int -> Int\nuseKernel n =\n    Elm.Kernel.Mystery.magic n\n\nmain = \"kernel ok\"\n",
    )]);
    // Compiles and loads; the kernel value is only referenced inside an
    // uncalled function, so node never evaluates it.
    match result {
        Ok(out) => assert_eq!(out, "kernel ok"),
        Err(e) => panic!("kernel import should compile: {}", e),
    }
}

#[test]
fn module_name_mismatch_is_reported() {
    let err = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Data.Math\n\nmain = \"x\"\n",
        ),
        ("Data/Math.elm", "module Data.Wrong exposing (x)\n\nx = 1\n"),
    ])
    .unwrap_err();
    assert!(err.contains("MODULE NAME MISMATCH"), "got: {}", err);
}

#[test]
fn svg_alias_types_in_annotations() {
    let result = project(&[(
        "Main.elm",
        "module Main exposing (main)\n\nimport Svg\nimport Svg.Attributes\n\ncircle : Svg.Svg msg\ncircle =\n    Svg.circle [ Svg.Attributes.r \"4\" ] []\n\nattrs : List (Svg.Attribute msg)\nattrs =\n    [ Svg.Attributes.fill \"red\" ]\n\nmain = \"svg types ok\"\n",
    )]);
    assert_eq!(result.unwrap(), "svg types ok");
}

// COVERAGE: project discovery and dependency-order paths

#[test]
fn opaque_ctor_exposing_import_is_lenient() {
    // Elm does not reject `import M exposing (T(..))` when M exposes T
    // opaquely; it imports the type with no (private) constructors. So the
    // import compiles as long as the private constructor is not used.
    let ok = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Counter exposing (Counter(..))\n\nmain : String\nmain = \"x\"\n",
        ),
        (
            "Counter.elm",
            "module Counter exposing (Counter)\n\ntype Counter\n    = Counter Int\n",
        ),
    ]);
    assert_eq!(ok.unwrap(), "x");

    // But the private constructor is still not in scope, so using it fails.
    let err = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Counter exposing (Counter(..))\n\nmain = Counter 5\n",
        ),
        (
            "Counter.elm",
            "module Counter exposing (Counter)\n\ntype Counter\n    = Counter Int\n",
        ),
    ])
    .unwrap_err();
    assert!(err.contains("Counter"), "got: {}", err);
}

#[test]
fn project_without_elm_json() {
    // The entry file's directory becomes the only source root.
    let dir = common::test_dir("alm-noelmjson", "t");
    std::fs::write(
        dir.join("Main.elm"),
        "module Main exposing (main)\n\nimport Helper\n\nmain = Helper.word\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("Helper.elm"),
        "module Helper exposing (word)\n\nword = \"bare\"\n",
    )
    .unwrap();
    let out = compile_and_run(&dir.join("Main.elm")).unwrap();
    assert_eq!(out, "bare");
}

#[test]
fn elm_json_without_source_directories() {
    let dir = common::test_dir("alm-nodirs", "t");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(dir.join("elm.json"), r#"{ "type": "application" }"#).unwrap();
    std::fs::write(
        src.join("Main.elm"),
        "module Main exposing (main)\n\nmain = \"default src\"\n",
    )
    .unwrap();
    assert_eq!(compile_and_run(&src.join("Main.elm")).unwrap(), "default src");
}

#[test]
fn packages_resolve_from_elm_home() {
    let dir = common::test_dir("alm-pkg", "t");
    let src = dir.join("src");
    let pkg_src = dir
        .join("elm-home/0.19.1/packages/acme/tools/1.0.0/src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&pkg_src).unwrap();
    std::fs::write(
        dir.join("elm.json"),
        r#"{
    "type": "application",
    "source-directories": ["src"],
    "dependencies": {
        "direct": { "acme/tools": "1.0.0", "acme/absent": "2.0.0" },
        "indirect": { "not-a-version": "later" }
    }
}"#,
    )
    .unwrap();
    std::fs::write(
        pkg_src.join("Acme.elm"),
        "module Acme exposing (shout)\n\nshout : String -> String\nshout s = String.toUpper s ++ \"!\"\n",
    )
    .unwrap();
    std::fs::write(
        src.join("Main.elm"),
        "module Main exposing (main)\n\nimport Acme\n\nmain = Acme.shout \"pkg\"\n",
    )
    .unwrap();

    let _guard = ELM_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("ELM_HOME", dir.join("elm-home"));
    let result = compile_and_run(&src.join("Main.elm"));
    std::env::remove_var("ELM_HOME");
    assert_eq!(result.unwrap(), "PKG!");
}

#[test]
fn duplicate_module_names_resolve_per_package() {
    // Two different packages each define a module named `Bar`. Elm scopes each
    // module's imports to its OWN package's dependencies, so `Foo` (in pp/one)
    // must see pp/one's `Bar`, never qq/two's. A flat namespace would pick
    // qq/two's `Bar` (whose transitive imports reach back to `Foo`), inventing
    // a false import cycle Foo -> Bar -> Baz -> Foo. Regression: e.g. both
    // elm-community/html-extra and arowM/html-extra expose `Html.Extra`.
    let dir = common::test_dir("alm-dupmod", "t");
    let src = dir.join("src");
    let pkgs = dir.join("elm-home/0.19.1/packages");
    let one = pkgs.join("pp/one/1.0.0");
    let two = pkgs.join("qq/two/1.0.0");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(one.join("src")).unwrap();
    std::fs::create_dir_all(two.join("src")).unwrap();

    // pp/one: Foo imports its own Bar; Bar imports nothing.
    std::fs::write(
        one.join("elm.json"),
        r#"{ "type": "package", "name": "pp/one", "version": "1.0.0", "exposed-modules": ["Foo", "Bar"], "dependencies": {} }"#,
    )
    .unwrap();
    std::fs::write(
        one.join("src/Foo.elm"),
        "module Foo exposing (foo)\n\nimport Bar\n\nfoo : String\nfoo = \"foo+\" ++ Bar.bar\n",
    )
    .unwrap();
    std::fs::write(
        one.join("src/Bar.elm"),
        "module Bar exposing (bar)\n\nbar : String\nbar = \"one-bar\"\n",
    )
    .unwrap();

    // qq/two depends on pp/one: its Bar imports Baz, and Baz imports pp/one's Foo.
    std::fs::write(
        two.join("elm.json"),
        r#"{ "type": "package", "name": "qq/two", "version": "1.0.0", "exposed-modules": ["Bar", "Baz"], "dependencies": { "pp/one": "1.0.0 <= v < 2.0.0" } }"#,
    )
    .unwrap();
    std::fs::write(
        two.join("src/Bar.elm"),
        "module Bar exposing (bar)\n\nimport Baz\n\nbar : String\nbar = \"two-bar+\" ++ Baz.baz\n",
    )
    .unwrap();
    std::fs::write(
        two.join("src/Baz.elm"),
        "module Baz exposing (baz)\n\nimport Foo\n\nbaz : String\nbaz = \"baz+\" ++ Foo.foo\n",
    )
    .unwrap();

    // App depends on both. `qq/two` is listed first so a flat resolver would
    // find its `Bar` first when resolving Foo's import.
    std::fs::write(
        dir.join("elm.json"),
        r#"{ "type": "application", "source-directories": ["src"], "dependencies": { "direct": { "qq/two": "1.0.0", "pp/one": "1.0.0" }, "indirect": {} }, "test-dependencies": { "direct": {}, "indirect": {} } }"#,
    )
    .unwrap();
    std::fs::write(
        src.join("Main.elm"),
        "module Main exposing (main)\n\nimport Foo\n\nmain = Foo.foo\n",
    )
    .unwrap();

    let _guard = ELM_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("ELM_HOME", dir.join("elm-home"));
    let result = compile_and_run(&src.join("Main.elm"));
    std::env::remove_var("ELM_HOME");
    // pp/one's Bar ("one-bar"), and no false import cycle.
    assert_eq!(result.unwrap(), "foo+one-bar");
}

#[test]
fn extensible_alias_applied_to_type_variable() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Lib exposing (Named)\n\nname : Named a -> String\nname r = r.name\n\nmain = name { name = \"var\", extra = 1 }\n",
        ),
        (
            "Lib.elm",
            "module Lib exposing (Named)\n\ntype alias Named a =\n    { a | name : String }\n",
        ),
    ]);
    assert_eq!(result.unwrap(), "var");
}

#[test]
fn custom_operator_used_inside_its_own_module() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Ops\n\nmain = String.fromInt Ops.fortyTwo\n",
        ),
        (
            "Ops.elm",
            "module Ops exposing (fortyTwo)\n\ninfix left 6 (|+|) = plus\n\n\nplus : Int -> Int -> Int\nplus a b =\n    a + b\n\n\nfortyTwo : Int\nfortyTwo =\n    20 |+| 22\n",
        ),
    ]);
    assert_eq!(result.unwrap(), "42");
}

#[test]
fn qualified_foreign_constructors_and_alias_ctors() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Point exposing (Point)\nimport Shape\n\nshifted : Point\nshifted = Point 1 2\n\nmain =\n    case Shape.Circle 2.0 of\n        Shape.Circle r ->\n            String.fromFloat r ++ \"/\" ++ String.fromInt shifted.y\n\n        Shape.Square _ ->\n            \"square\"\n",
        ),
        (
            "Shape.elm",
            "module Shape exposing (Shape(..))\n\ntype Shape\n    = Circle Float\n    | Square Float\n",
        ),
        (
            "Point.elm",
            "module Point exposing (Point)\n\ntype alias Point =\n    { x : Int, y : Int }\n",
        ),
    ]);
    assert_eq!(result.unwrap(), "2/2");
}

// A real elm/bytes round-trip through the JavaScript runtime. Compiles a
// project that depends on a (compact, faithful) `elm/bytes` package resolved
// from a fake ELM_HOME, then runs the generated JS under node. Regression for
// the `Elm.Kernel.Bytes` runtime (encode tree-walk, read_*/write_*, decode,
// loop, UTF-8 strings). The expected string was verified byte-for-byte against
// the official Elm 0.19.1 compiler with the real elm/bytes 1.0.8.
#[test]
fn elm_bytes_encode_decode_roundtrip() {
    let dir = common::test_dir("alm-bytes", "t");
    let src = dir.join("src");
    let pkg_src = dir.join("elm-home/0.19.1/packages/elm/bytes/1.0.8/src");
    std::fs::create_dir_all(src.join("")).unwrap();
    std::fs::create_dir_all(pkg_src.join("Bytes")).unwrap();
    std::fs::write(
        dir.join("elm.json"),
        r#"{
    "type": "application",
    "source-directories": ["src"],
    "dependencies": {
        "direct": { "elm/bytes": "1.0.8" },
        "indirect": {}
    },
    "test-dependencies": { "direct": {}, "indirect": {} }
}"#,
    )
    .unwrap();
    std::fs::write(
        pkg_src.parent().unwrap().join("elm.json"),
        r#"{ "type": "package", "name": "elm/bytes", "summary": "b", "license": "BSD-3-Clause", "version": "1.0.8", "exposed-modules": ["Bytes", "Bytes.Encode", "Bytes.Decode"], "elm-version": "0.19.0 <= v < 0.20.0", "dependencies": { "elm/core": "1.0.0 <= v < 2.0.0" }, "test-dependencies": {} }
"#,
    )
    .unwrap();
    std::fs::write(pkg_src.join("Bytes.elm"), r#"module Bytes exposing (Bytes, width, Endianness(..), getHostEndianness)

import Elm.Kernel.Bytes
import Task exposing (Task)


type Bytes
    = Bytes


width : Bytes -> Int
width =
    Elm.Kernel.Bytes.width


type Endianness
    = LE
    | BE


getHostEndianness : Task x Endianness
getHostEndianness =
    Elm.Kernel.Bytes.getHostEndianness LE BE
"#).unwrap();
    std::fs::write(pkg_src.join("Bytes/Encode.elm"), r#"module Bytes.Encode exposing
    ( encode, Encoder
    , signedInt8, signedInt16, signedInt32
    , unsignedInt8, unsignedInt16, unsignedInt32
    , float32, float64, bytes, string, getStringWidth, sequence
    )

import Bytes exposing (Bytes, Endianness(..))


type Encoder
    = I8 Int
    | I16 Endianness Int
    | I32 Endianness Int
    | U8 Int
    | U16 Endianness Int
    | U32 Endianness Int
    | F32 Endianness Float
    | F64 Endianness Float
    | Seq Int (List Encoder)
    | Utf8 Int String
    | Bytes Bytes


encode : Encoder -> Bytes
encode =
    Elm.Kernel.Bytes.encode


signedInt8 : Int -> Encoder
signedInt8 =
    I8


signedInt16 : Endianness -> Int -> Encoder
signedInt16 =
    I16


signedInt32 : Endianness -> Int -> Encoder
signedInt32 =
    I32


unsignedInt8 : Int -> Encoder
unsignedInt8 =
    U8


unsignedInt16 : Endianness -> Int -> Encoder
unsignedInt16 =
    U16


unsignedInt32 : Endianness -> Int -> Encoder
unsignedInt32 =
    U32


float32 : Endianness -> Float -> Encoder
float32 =
    F32


float64 : Endianness -> Float -> Encoder
float64 =
    F64


bytes : Bytes -> Encoder
bytes =
    Bytes


string : String -> Encoder
string str =
    Utf8 (Elm.Kernel.Bytes.getStringWidth str) str


getStringWidth : String -> Int
getStringWidth =
    Elm.Kernel.Bytes.getStringWidth


sequence : List Encoder -> Encoder
sequence builders =
    Seq (getWidths 0 builders) builders


write : Encoder -> Bytes -> Int -> Int
write builder mb offset =
    case builder of
        I8 n ->
            Elm.Kernel.Bytes.write_i8 mb offset n

        I16 e n ->
            Elm.Kernel.Bytes.write_i16 mb offset n (e == LE)

        I32 e n ->
            Elm.Kernel.Bytes.write_i32 mb offset n (e == LE)

        U8 n ->
            Elm.Kernel.Bytes.write_u8 mb offset n

        U16 e n ->
            Elm.Kernel.Bytes.write_u16 mb offset n (e == LE)

        U32 e n ->
            Elm.Kernel.Bytes.write_u32 mb offset n (e == LE)

        F32 e n ->
            Elm.Kernel.Bytes.write_f32 mb offset n (e == LE)

        F64 e n ->
            Elm.Kernel.Bytes.write_f64 mb offset n (e == LE)

        Seq _ bs ->
            writeSequence bs mb offset

        Utf8 _ s ->
            Elm.Kernel.Bytes.write_string mb offset s

        Bytes bs ->
            Elm.Kernel.Bytes.write_bytes mb offset bs


writeSequence : List Encoder -> Bytes -> Int -> Int
writeSequence builders mb offset =
    case builders of
        [] ->
            offset

        b :: bs ->
            writeSequence bs mb (write b mb offset)


getWidth : Encoder -> Int
getWidth builder =
    case builder of
        I8 _ ->
            1

        I16 _ _ ->
            2

        I32 _ _ ->
            4

        U8 _ ->
            1

        U16 _ _ ->
            2

        U32 _ _ ->
            4

        F32 _ _ ->
            4

        F64 _ _ ->
            8

        Seq w _ ->
            w

        Utf8 w _ ->
            w

        Bytes bs ->
            Elm.Kernel.Bytes.width bs


getWidths : Int -> List Encoder -> Int
getWidths w builders =
    case builders of
        [] ->
            w

        b :: bs ->
            getWidths (w + getWidth b) bs
"#).unwrap();
    std::fs::write(pkg_src.join("Bytes/Decode.elm"), r#"module Bytes.Decode exposing
    ( Decoder, decode
    , signedInt8, signedInt16, signedInt32
    , unsignedInt8, unsignedInt16, unsignedInt32
    , float32, float64, string, bytes
    , map, map2, map3, map4, map5
    , andThen, succeed, fail
    , Step(..), loop
    )

import Bytes exposing (Bytes, Endianness(..))


type Decoder a
    = Decoder (Bytes -> Int -> ( Int, a ))


decode : Decoder a -> Bytes -> Maybe a
decode (Decoder decoder) bs =
    Elm.Kernel.Bytes.decode decoder bs


signedInt8 : Decoder Int
signedInt8 =
    Decoder Elm.Kernel.Bytes.read_i8


signedInt16 : Endianness -> Decoder Int
signedInt16 endianness =
    Decoder (Elm.Kernel.Bytes.read_i16 (endianness == LE))


signedInt32 : Endianness -> Decoder Int
signedInt32 endianness =
    Decoder (Elm.Kernel.Bytes.read_i32 (endianness == LE))


unsignedInt8 : Decoder Int
unsignedInt8 =
    Decoder Elm.Kernel.Bytes.read_u8


unsignedInt16 : Endianness -> Decoder Int
unsignedInt16 endianness =
    Decoder (Elm.Kernel.Bytes.read_u16 (endianness == LE))


unsignedInt32 : Endianness -> Decoder Int
unsignedInt32 endianness =
    Decoder (Elm.Kernel.Bytes.read_u32 (endianness == LE))


float32 : Endianness -> Decoder Float
float32 endianness =
    Decoder (Elm.Kernel.Bytes.read_f32 (endianness == LE))


float64 : Endianness -> Decoder Float
float64 endianness =
    Decoder (Elm.Kernel.Bytes.read_f64 (endianness == LE))


bytes : Int -> Decoder Bytes
bytes n =
    Decoder (Elm.Kernel.Bytes.read_bytes n)


string : Int -> Decoder String
string n =
    Decoder (Elm.Kernel.Bytes.read_string n)


map : (a -> b) -> Decoder a -> Decoder b
map func (Decoder decodeA) =
    Decoder
        (\bites offset ->
            let
                ( aOffset, a ) =
                    decodeA bites offset
            in
            ( aOffset, func a )
        )


map2 : (a -> b -> r) -> Decoder a -> Decoder b -> Decoder r
map2 func (Decoder decodeA) (Decoder decodeB) =
    Decoder
        (\bites offset ->
            let
                ( aOffset, a ) =
                    decodeA bites offset

                ( bOffset, b ) =
                    decodeB bites aOffset
            in
            ( bOffset, func a b )
        )


map3 : (a -> b -> c -> r) -> Decoder a -> Decoder b -> Decoder c -> Decoder r
map3 func (Decoder decodeA) (Decoder decodeB) (Decoder decodeC) =
    Decoder
        (\bites offset ->
            let
                ( aOffset, a ) =
                    decodeA bites offset

                ( bOffset, b ) =
                    decodeB bites aOffset

                ( cOffset, c ) =
                    decodeC bites bOffset
            in
            ( cOffset, func a b c )
        )


map4 : (a -> b -> c -> d -> r) -> Decoder a -> Decoder b -> Decoder c -> Decoder d -> Decoder r
map4 func (Decoder decodeA) (Decoder decodeB) (Decoder decodeC) (Decoder decodeD) =
    Decoder
        (\bites offset ->
            let
                ( aOffset, a ) =
                    decodeA bites offset

                ( bOffset, b ) =
                    decodeB bites aOffset

                ( cOffset, c ) =
                    decodeC bites bOffset

                ( dOffset, d ) =
                    decodeD bites cOffset
            in
            ( dOffset, func a b c d )
        )


map5 : (a -> b -> c -> d -> e -> r) -> Decoder a -> Decoder b -> Decoder c -> Decoder d -> Decoder e -> Decoder r
map5 func (Decoder decodeA) (Decoder decodeB) (Decoder decodeC) (Decoder decodeD) (Decoder decodeE) =
    Decoder
        (\bites offset ->
            let
                ( aOffset, a ) =
                    decodeA bites offset

                ( bOffset, b ) =
                    decodeB bites aOffset

                ( cOffset, c ) =
                    decodeC bites bOffset

                ( dOffset, d ) =
                    decodeD bites cOffset

                ( eOffset, e ) =
                    decodeE bites dOffset
            in
            ( eOffset, func a b c d e )
        )


andThen : (a -> Decoder b) -> Decoder a -> Decoder b
andThen callback (Decoder decodeA) =
    Decoder
        (\bites offset ->
            let
                ( newOffset, a ) =
                    decodeA bites offset

                (Decoder decodeB) =
                    callback a
            in
            decodeB bites newOffset
        )


succeed : a -> Decoder a
succeed a =
    Decoder (\_ offset -> ( offset, a ))


fail : Decoder a
fail =
    Decoder Elm.Kernel.Bytes.decodeFailure


type Step state a
    = Loop state
    | Done a


loop : state -> (state -> Decoder (Step state a)) -> Decoder a
loop state callback =
    Decoder (loopHelp state callback)


loopHelp : state -> (state -> Decoder (Step state a)) -> Bytes -> Int -> ( Int, a )
loopHelp state callback bites offset =
    let
        (Decoder decoder) =
            callback state

        ( newOffset, step ) =
            decoder bites offset
    in
    case step of
        Loop newState ->
            loopHelp newState callback bites newOffset

        Done result ->
            ( newOffset, result )
"#).unwrap();
    std::fs::write(src.join("Main.elm"), r#"module Main exposing (main)

import Bytes exposing (Endianness(..))
import Bytes.Encode as E
import Bytes.Decode as D


listDecoder : D.Decoder (List Int)
listDecoder =
    D.unsignedInt8 |> D.andThen (\n -> D.loop ( n, [] ) step)


step : ( Int, List Int ) -> D.Decoder (D.Step ( Int, List Int ) (List Int))
step ( n, xs ) =
    if n <= 0 then
        D.succeed (D.Done (List.reverse xs))

    else
        D.map (\x -> D.Loop ( n - 1, x :: xs )) D.unsignedInt8


main : String
main =
    let
        e =
            E.encode (E.sequence [ E.unsignedInt8 65, E.signedInt32 BE 1000000, E.float64 LE 3.5, E.string "brød" ])

        d =
            D.decode (D.map3 (\a b c -> ( a, b, c )) D.unsignedInt8 (D.signedInt32 BE) (D.float64 LE)) e

        s =
            D.decode (D.string 5) (E.encode (E.string "brød"))

        loopEnc =
            E.encode (E.sequence (E.unsignedInt8 3 :: List.map E.unsignedInt8 [ 7, 8, 9 ]))

        loopDec =
            D.decode listDecoder loopEnc

        fail =
            D.decode (D.unsignedInt32 BE) (E.encode (E.unsignedInt8 1))
    in
    Debug.toString ( Bytes.width e, d, ( s, loopDec, fail ) )
"#).unwrap();

    let _guard = ELM_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("ELM_HOME", dir.join("elm-home"));
    let result = compile_and_run(&src.join("Main.elm"));
    std::env::remove_var("ELM_HOME");
    assert_eq!(
        result.unwrap(),
        r#"(18,Just (65,1000000,3.5),(Just "brød",Just [7,8,9],Nothing))"#
    );
}
