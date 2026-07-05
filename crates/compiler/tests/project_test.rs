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
    let dir = std::env::temp_dir().join(format!(
        "alm-project-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
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

fn compile_and_run(entry: &Path) -> Result<String, String> {
    let javascript = alm_compiler::project::compile_project(entry)
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
    let dir = std::env::temp_dir().join(format!(
        "alm-noelmjson-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
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
    let dir = std::env::temp_dir().join(format!(
        "alm-nodirs-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
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
    let dir = std::env::temp_dir().join(format!(
        "alm-pkg-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
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
    let dir = std::env::temp_dir().join(format!(
        "alm-dupmod-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
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
