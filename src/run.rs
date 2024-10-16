use crate::assert::Assert;
use lazy_static::lazy_static;
use regex::Regex;
use std::{
    borrow::Cow, collections::HashMap, env, error::Error, ffi::OsString, io::prelude::*,
    path::PathBuf, process::Command,
};

#[doc(hidden)]
pub enum Language {
    C,
    Cxx,
}

impl ToString for Language {
    fn to_string(&self) -> String {
        match self {
            Self::C => String::from("c"),
            Self::Cxx => String::from("cpp"),
        }
    }
}

#[doc(hidden)]
pub fn run(language: Language, program: &str) -> Result<Assert, Box<dyn Error>> {
    let (program, variables) = collect_environment_variables(program);

    let mut program_file = tempfile::Builder::new()
        .prefix("inline-c-rs-")
        .suffix(&format!(".{}", language.to_string()))
        .tempfile()?;
    program_file.write_all(program.as_bytes())?;

    let host = target_lexicon::HOST.to_string();
    let target = &host;

    let msvc = target.contains("msvc");

    let (_, input_path) = program_file.keep()?;
    let mut output_temp = tempfile::Builder::new();
    let output_temp = output_temp.prefix("inline-c-rs-");

    if msvc {
        output_temp.suffix(".exe");
    }

    let (_, output_path) = output_temp.tempfile()?.keep()?;

    let mut build = cc::Build::new();
    let mut build = build
        .cargo_metadata(false)
        .warnings(true)
        .extra_warnings(true)
        .warnings_into_errors(true)
        .debug(false)
        .host(&host)
        .target(target)
        .opt_level(1);

    if let Language::Cxx = language {
        build = build.cpp(true);
    }

    // Usually, `cc-rs` is used to produce libraries. In our case, we
    // want to produce an (executable) object file. The following code
    // is kind of a hack around `cc-rs`. It avoids the addition of the
    // `-c` argument on the compiler, and manually adds other
    // arguments.

    let compiler = build.try_get_compiler()?;
    let mut command;

    if msvc {
        command = compiler.to_command();

        command_add_compiler_flags(&mut command, &variables, msvc);
        command_add_output_file(&mut command, &output_path, msvc, compiler.is_like_clang());
        command.arg(input_path.clone());
        command_add_linker_flags_msvc(&mut command, &variables);
    } else {
        command = Command::new(compiler.path());

        command.arg(input_path.clone()); // the input must come first
        command.args(compiler.args());
        command_add_compiler_flags(&mut command, &variables, msvc);
        command_add_output_file(&mut command, &output_path, msvc, compiler.is_like_clang());
    }

    command.envs(variables.clone());

    let mut files_to_remove = vec![input_path, output_path.clone()];
    if msvc {
        let mut intermediate_path = output_path.clone();
        intermediate_path.set_extension("obj");
        files_to_remove.push(intermediate_path);
    }

    let clang_output = command.output()?;

    if !clang_output.status.success() {
        return Ok(Assert::new(command, Some(files_to_remove)));
    }

    let mut command = Command::new(output_path);
    command.envs(variables);

    Ok(Assert::new(command, Some(files_to_remove)))
}

fn collect_environment_variables<'p>(program: &'p str) -> (Cow<'p, str>, HashMap<String, String>) {
    const ENV_VAR_PREFIX: &str = "INLINE_C_RS_";

    lazy_static! {
        static ref REGEX: Regex = Regex::new(
            r#"#inline_c_rs (?P<variable_name>[^:]+):\s*"(?P<variable_value>[^"]+)"\r?\n"#
        )
        .unwrap();
    }

    let mut variables = HashMap::new();

    for (variable_name, variable_value) in env::vars().filter_map(|(mut name, value)| {
        if name.starts_with(ENV_VAR_PREFIX) {
            Some((name.split_off(ENV_VAR_PREFIX.len()), value))
        } else {
            None
        }
    }) {
        variables.insert(variable_name, variable_value);
    }

    for captures in REGEX.captures_iter(program) {
        variables.insert(
            captures["variable_name"].trim().to_string(),
            captures["variable_value"].to_string(),
        );
    }

    let program = REGEX.replace_all(program, "");

    (program, variables)
}

// This is copy-pasted and edited from `cc-rs`.
fn command_add_output_file(command: &mut Command, output_path: &PathBuf, msvc: bool, clang: bool) {
    if msvc && !clang {
        let mut intermediate_path = output_path.clone();
        intermediate_path.set_extension("obj");

        let mut fo_arg = OsString::from("-Fo");
        fo_arg.push(intermediate_path);
        command.arg(fo_arg);

        let mut fe_arg = OsString::from("-Fe");
        fe_arg.push(output_path);
        command.arg(fe_arg);
    } else {
        command.arg("-o").arg(output_path);
    }
}

fn get_env_flags(variables: &HashMap<String, String>, env_name: &str) -> Vec<String> {
    variables
        .get(env_name)
        .map(|e| e.to_string())
        .ok_or_else(|| env::var(env_name))
        .unwrap_or_default()
        .split_ascii_whitespace()
        .map(|slice| slice.to_string())
        .collect()
}

fn command_add_compiler_flags(
    command: &mut Command,
    variables: &HashMap<String, String>,
    msvc: bool,
) {
    command.args(get_env_flags(variables, "CFLAGS"));
    command.args(get_env_flags(variables, "CPPFLAGS"));
    command.args(get_env_flags(variables, "CXXFLAGS"));

    if !msvc {
        for linker_argument in get_env_flags(variables, "LDFLAGS") {
            command.arg(format!("-Wl,{}", linker_argument));
        }
    }
}

fn command_add_linker_flags_msvc(command: &mut Command, variables: &HashMap<String, String>) {
    let linker_flags = get_env_flags(variables, "LDFLAGS");
    if !linker_flags.is_empty() {
        command.arg("/link");
        for linker_argument in linker_flags {
            command.arg(linker_argument);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::predicates::*;

    #[test]
    fn test_run_c() {
        run(
            Language::C,
            r#"
                #include <stdio.h>

                int main() {
                    printf("Hello, World!\n");

                    return 0;
                }
            "#,
        )
        .unwrap()
        .success()
        .stdout(predicate::eq("Hello, World!\n").normalize());
    }

    #[cfg(not(target_os = "macos"))] // macOS doesn't support `--defsym`
    #[test]
    fn test_run_c_ldflags() {
        use std::env::{remove_var, set_var};

        let host = target_lexicon::HOST.to_string();
        let msvc = host.contains("msvc");
        if msvc {
            // Use a linker flag to set the stack size and run a program that
            // outputs its stack size. If the linker flags aren't set
            // properly, the program will output the wrong value.

            set_var("INLINE_C_RS_LDFLAGS", "/STACK:0x40000");
            run(
                Language::C,
                r#"
                #include <Windows.h>

                #include <stdio.h>

                int main() {
                    ULONG_PTR stack_low_limit, stack_high_limit;
                    GetCurrentThreadStackLimits(&stack_low_limit, &stack_high_limit);
                    printf("%#llx", stack_high_limit-stack_low_limit);

                    return 0;
                }
            "#,
            )
            .unwrap()
            .success()
            .stdout(predicate::eq("0x40000").normalize());

            remove_var("INLINE_C_RS_LDFLAGS");
        } else {
            // Introduces a symbol using linker flags and builds a program that
            // ODR uses that symbol. If the linker flags aren't set properly,
            // there will be a linker error.

            set_var("INLINE_C_RS_LDFLAGS", "--defsym MY_SYMBOL=0x0");
            run(
                Language::C,
                r#"
                #include <stdio.h>

                extern void* MY_SYMBOL;

                int main() {
                    printf("%p", MY_SYMBOL);
                    return 0;
                }
            "#,
            )
            .unwrap()
            .success();

            remove_var("INLINE_C_RS_LDFLAGS");
        }
    }

    #[test]
    fn test_run_cxx() {
        run(
            Language::Cxx,
            r#"
                #include <stdio.h>

                int main() {
                    printf("Hello, World!\n");

                    return 0;
                }
            "#,
        )
        .unwrap()
        .success()
        .stdout(predicate::eq("Hello, World!\n").normalize());
    }
}
