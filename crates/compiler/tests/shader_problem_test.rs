//! The vendored GLSL parser (the `glsl` crate) rejects malformed shader blocks
//! with a SHADER PROBLEM report. Unlike the rest of `parse_error_test`, this is
//! NOT byte-exact with `elm make` — elm embeds a different (parsec) parser's
//! message — so it is checked structurally here.
#[test]
fn bad_glsl_is_a_shader_problem() {
    let src = "module Main exposing (..)\n\nx = [glsl| this is not glsl |]\n";
    let out = match alm_compiler::compile(src) {
        Ok(_) => panic!("expected a shader problem"),
        Err(reports) => reports
            .iter()
            .map(|r| r.render("src/Main.elm", src))
            .collect::<String>(),
    };
    assert!(out.contains("-- SHADER PROBLEM"), "got:\n{out}");
    assert!(out.contains("I ran into a problem while parsing this GLSL block."), "got:\n{out}");
    assert!(out.contains("3rd party GLSL parser"), "got:\n{out}");
}

#[test]
fn valid_glsl_compiles() {
    // A real elm-webgl shader (GLSL ES 1.00: attribute/uniform/varying) must
    // still be accepted.
    let src = "module Main exposing (..)\n\nvs =\n    [glsl| attribute vec3 position; uniform mat4 m; varying vec3 c; void main () { gl_Position = m * vec4(position, 1.0); c = position; } |]\n";
    assert!(alm_compiler::compile(src).is_ok());
}
