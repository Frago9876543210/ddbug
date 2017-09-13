#[macro_use]
extern crate clap;
extern crate env_logger;
#[macro_use]
extern crate log;

extern crate ddbug;

// Mode
const OPT_FILE: &'static str = "file";
const OPT_DIFF: &'static str = "diff";

// Print categories
const OPT_CATEGORY: &'static str = "category";
const OPT_CATEGORY_FILE: &'static str = "file";
const OPT_CATEGORY_UNIT: &'static str = "unit";
const OPT_CATEGORY_TYPE: &'static str = "type";
const OPT_CATEGORY_FUNCTION: &'static str = "function";
const OPT_CATEGORY_VARIABLE: &'static str = "variable";

// Print fields
const OPT_PRINT: &'static str = "print";
const OPT_PRINT_FILE_ADDRESS: &'static str = "file-address";
const OPT_PRINT_UNIT_ADDRESS: &'static str = "unit-address";
const OPT_PRINT_FUNCTION_CALLS: &'static str = "function-calls";
const OPT_PRINT_FUNCTION_VARIABLES: &'static str = "function-variables";

// Print parameters
const OPT_INLINE_DEPTH: &'static str = "inline-depth";

// Filters
const OPT_FILTER: &'static str = "filter";
const OPT_FILTER_INLINE: &'static str = "inline";
const OPT_FILTER_FUNCTION_INLINE: &'static str = "function-inline";
const OPT_FILTER_NAME: &'static str = "name";
const OPT_FILTER_NAMESPACE: &'static str = "namespace";
const OPT_FILTER_UNIT: &'static str = "unit";

// Sorting
const OPT_SORT: &'static str = "sort";
const OPT_SORT_SIZE: &'static str = "size";
const OPT_SORT_NAME: &'static str = "name";

// Diff options
const OPT_IGNORE: &'static str = "ignore";
const OPT_IGNORE_ADDED: &'static str = "added";
const OPT_IGNORE_DELETED: &'static str = "deleted";
const OPT_IGNORE_FUNCTION_ADDRESS: &'static str = "function-address";
const OPT_IGNORE_FUNCTION_SIZE: &'static str = "function-size";
const OPT_IGNORE_FUNCTION_INLINE: &'static str = "function-inline";
const OPT_IGNORE_VARIABLE_ADDRESS: &'static str = "variable-address";
const OPT_PREFIX_MAP: &'static str = "prefix-map";

fn main() {
    env_logger::init().ok();

    let matches = clap::App::new("ddbug")
        .version(crate_version!())
        .setting(clap::AppSettings::UnifiedHelpMessage)
        .arg(
            clap::Arg::with_name(OPT_FILE)
                .help("Path of file to print")
                .value_name("FILE")
                .index(1)
                .required_unless(OPT_DIFF)
                .conflicts_with(OPT_DIFF),
        )
        .arg(
            clap::Arg::with_name(OPT_DIFF)
                .short("d")
                .long(OPT_DIFF)
                .help("Print difference between two files")
                .value_names(&["FILE", "FILE"]),
        )
        .arg(
            clap::Arg::with_name(OPT_CATEGORY)
                .short("c")
                .long(OPT_CATEGORY)
                .help("Categories of entries to print (defaults to all)")
                .takes_value(true)
                .multiple(true)
                .require_delimiter(true)
                .value_name("CATEGORY")
                .possible_values(&[
                    OPT_CATEGORY_FILE,
                    OPT_CATEGORY_UNIT,
                    OPT_CATEGORY_TYPE,
                    OPT_CATEGORY_FUNCTION,
                    OPT_CATEGORY_VARIABLE,
                ]),
        )
        .arg(
            clap::Arg::with_name(OPT_PRINT)
                .short("p")
                .long(OPT_PRINT)
                .help("Print extra fields within entries")
                .takes_value(true)
                .multiple(true)
                .require_delimiter(true)
                .value_name("FIELD")
                .possible_values(&[
                    OPT_PRINT_FILE_ADDRESS,
                    OPT_PRINT_UNIT_ADDRESS,
                    OPT_PRINT_FUNCTION_CALLS,
                    OPT_PRINT_FUNCTION_VARIABLES,
                ]),
        )
        .arg(
            clap::Arg::with_name(OPT_INLINE_DEPTH)
                .long(OPT_INLINE_DEPTH)
                .help("Depth of inlined function calls to print (defaults to 1, 0 to disable)")
                .value_name("DEPTH"),
        )
        .arg(
            clap::Arg::with_name(OPT_FILTER)
                .short("f")
                .long(OPT_FILTER)
                .help("Print only entries that match the given filters")
                .takes_value(true)
                .multiple(true)
                .require_delimiter(true)
                .value_name("FILTER"),
        )
        .arg(
            clap::Arg::with_name(OPT_SORT)
                .short("s")
                .long(OPT_SORT)
                .help("Sort entries by the given key")
                .takes_value(true)
                .value_name("KEY")
                .possible_values(&[OPT_SORT_NAME, OPT_SORT_SIZE]),
        )
        .arg(
            clap::Arg::with_name(OPT_IGNORE)
                .short("i")
                .long(OPT_IGNORE)
                .help("Don't print differences due to the given types of changes")
                .requires(OPT_DIFF)
                .takes_value(true)
                .multiple(true)
                .require_delimiter(true)
                .value_name("CHANGE")
                .possible_values(&[
                    OPT_IGNORE_ADDED,
                    OPT_IGNORE_DELETED,
                    OPT_IGNORE_FUNCTION_ADDRESS,
                    OPT_IGNORE_FUNCTION_SIZE,
                    OPT_IGNORE_FUNCTION_INLINE,
                    OPT_IGNORE_VARIABLE_ADDRESS,
                ]),
        )
        .arg(
            clap::Arg::with_name(OPT_PREFIX_MAP)
                .long(OPT_PREFIX_MAP)
                .help("When comparing file paths, replace the 'old' prefix with the 'new' prefix")
                .requires(OPT_DIFF)
                .takes_value(true)
                .multiple(true)
                .require_delimiter(true)
                .value_name("OLD>=<NEW"),
        )
        .after_help(concat!(
            "FILTERS:\n",
            "    function-inline=<yes|no>        Match function 'inline' value\n",
            "    name=<string>                   Match entries with the given name\n",
            "    namespace=<string>              Match entries within the given namespace\n",
            "    unit=<string>                   Match entries within the given unit\n"
        ))
        .get_matches();

    let mut options = ddbug::Options::default();

    options.inline_depth = if let Some(inline_depth) = matches.value_of(OPT_INLINE_DEPTH) {
        match inline_depth.parse::<usize>() {
            Ok(inline_depth) => inline_depth,
            Err(_) => {
                clap::Error::with_description(
                    &format!("invalid {} value: {}", OPT_INLINE_DEPTH, inline_depth),
                    clap::ErrorKind::InvalidValue,
                ).exit();
            }
        }
    } else {
        1
    };

    if let Some(values) = matches.values_of(OPT_CATEGORY) {
        for value in values {
            match value {
                OPT_CATEGORY_FILE => options.category_file = true,
                OPT_CATEGORY_UNIT => options.category_unit = true,
                OPT_CATEGORY_TYPE => options.category_type = true,
                OPT_CATEGORY_FUNCTION => options.category_function = true,
                OPT_CATEGORY_VARIABLE => options.category_variable = true,
                _ => clap::Error::with_description(
                    &format!("invalid {} value: {}", OPT_CATEGORY, value),
                    clap::ErrorKind::InvalidValue,
                ).exit(),
            }
        }
    } else {
        options.category_file = true;
        options.category_unit = true;
        options.category_type = true;
        options.category_function = true;
        options.category_variable = true;
    }

    if let Some(values) = matches.values_of(OPT_PRINT) {
        for value in values {
            match value {
                OPT_PRINT_FILE_ADDRESS => options.print_file_address = true,
                OPT_PRINT_UNIT_ADDRESS => options.print_unit_address = true,
                OPT_PRINT_FUNCTION_CALLS => options.print_function_calls = true,
                OPT_PRINT_FUNCTION_VARIABLES => options.print_function_variables = true,
                _ => clap::Error::with_description(
                    &format!("invalid {} value: {}", OPT_PRINT, value),
                    clap::ErrorKind::InvalidValue,
                ).exit(),
            }
        }
    } else {
        options.print_file_address = false;
        options.print_unit_address = false;
        options.print_function_calls = false;
        options.print_function_variables = false;
    }

    if let Some(values) = matches.values_of(OPT_FILTER) {
        for value in values {
            if let Some(index) = value.bytes().position(|c| c == b'=') {
                let key = &value[..index];
                let value = &value[index + 1..];
                match key {
                    OPT_FILTER_INLINE | OPT_FILTER_FUNCTION_INLINE => {
                        options.filter_function_inline = match value {
                            "y" | "yes" => Some(true),
                            "n" | "no" => Some(false),
                            _ => clap::Error::with_description(
                                &format!("invalid {} {} value: {}", OPT_FILTER, key, value),
                                clap::ErrorKind::InvalidValue,
                            ).exit(),
                        };
                    }
                    OPT_FILTER_NAME => options.filter_name = Some(value),
                    OPT_FILTER_NAMESPACE => options.filter_namespace = value.split("::").collect(),
                    OPT_FILTER_UNIT => options.filter_unit = Some(value),
                    _ => clap::Error::with_description(
                        &format!("invalid {} key: {}", OPT_FILTER, key),
                        clap::ErrorKind::InvalidValue,
                    ).exit(),
                }
            } else {
                clap::Error::with_description(
                    &format!("missing {} value for key: {}", OPT_FILTER, value),
                    clap::ErrorKind::InvalidValue,
                ).exit();
            }
        }
    }

    options.sort = match matches.value_of(OPT_SORT) {
        Some(OPT_SORT_NAME) => ddbug::Sort::Name,
        Some(OPT_SORT_SIZE) => ddbug::Sort::Size,
        Some(value) => clap::Error::with_description(
            &format!("invalid {} key: {}", OPT_SORT, value),
            clap::ErrorKind::InvalidValue,
        ).exit(),
        _ => ddbug::Sort::None,
    };

    if let Some(values) = matches.values_of(OPT_IGNORE) {
        for value in values {
            match value {
                OPT_IGNORE_ADDED => options.ignore_added = true,
                OPT_IGNORE_DELETED => options.ignore_deleted = true,
                OPT_IGNORE_FUNCTION_ADDRESS => options.ignore_function_address = true,
                OPT_IGNORE_FUNCTION_SIZE => options.ignore_function_size = true,
                OPT_IGNORE_FUNCTION_INLINE => options.ignore_function_inline = true,
                OPT_IGNORE_VARIABLE_ADDRESS => options.ignore_variable_address = true,
                _ => clap::Error::with_description(
                    &format!("invalid {} value: {}", OPT_IGNORE, value),
                    clap::ErrorKind::InvalidValue,
                ).exit(),
            }
        }
    }

    if let Some(values) = matches.values_of(OPT_PREFIX_MAP) {
        for value in values {
            if let Some(index) = value.bytes().position(|c| c == b'=') {
                let old = &value[..index];
                let new = &value[index + 1..];
                options.prefix_map.push((old, new));
            } else {
                clap::Error::with_description(
                    &format!("invalid {} value: {}", OPT_PREFIX_MAP, value),
                    clap::ErrorKind::InvalidValue,
                ).exit();
            }
        }
        options.prefix_map.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    }

    if let Some(mut paths) = matches.values_of(OPT_DIFF) {
        let path_a = paths.next().unwrap();
        let path_b = paths.next().unwrap();

        if let Err(e) = ddbug::File::parse(path_a, &mut |file_a| {
            if let Err(e) =
                ddbug::File::parse(path_b, &mut |file_b| diff_file(file_a, file_b, &options))
            {
                error!("{}: {}", path_b, e);
            }
            Ok(())
        }) {
            error!("{}: {}", path_a, e);
        }
    } else {
        let path = matches.value_of(OPT_FILE).unwrap();

        if let Err(e) = ddbug::File::parse(path, &mut |file| print_file(file, &options)) {
            error!("{}: {}", path, e);
        }
    }
}

fn diff_file(
    file_a: &mut ddbug::File,
    file_b: &mut ddbug::File,
    options: &ddbug::Options,
) -> ddbug::Result<()> {
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    if let Err(e) = ddbug::File::diff(&mut writer, file_a, file_b, options) {
        error!("{}", e);
    }
    Ok(())
}

fn print_file(file: &mut ddbug::File, options: &ddbug::Options) -> ddbug::Result<()> {
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    file.print(&mut writer, options)
}
