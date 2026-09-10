use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    ffi::OsString,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Arg, ArgAction, Args, Command, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use serde::Serialize;

use crate::{
    adapter::Adapter,
    auth::{self, AuthApi},
    cli_model::{
        ALLOW_BUILD_ENV, ALLOW_INSTALL_HOOKS_ENV, ALLOW_NATIVE_DEPS_ENV, ALLOW_NO_MANIFEST_ENV,
        AUTH_URL_ENV, CLI_ENV, CLI_INSTALL_MODE_ENV, CLI_TARGET_ENV, DO_NOT_WRITE_NEW_MANIFEST_ENV,
        FROZEN_ENV, GIT_SUBMODULES_ENV, HOME_ENV, INSTALL_MODE_ENV, INTERACTIVE_ENV, NAME_ENV,
        NATIVE_MANAGER_ENV, NO_MIRRORS_ENV, ORG_ENV, REGISTRY_ENV, SUPABASE_KEY_ENV,
        SUPABASE_URL_ENV, TARGET_ENV, TOKEN_ENV, TRUST_MIRROR_METADATA_ENV,
    },
    commands::{self, GlobalOptions},
    environment,
    error::ZedError,
    install::{InstallMode, NativeDepsPolicy},
    mirror,
    package,
    r2g::{self, R2gRegistryMode},
    registry::{self, Registry},
};

const NON_CLAP_FLAG_ENVS: &[&str] = &["ZED_PKG_COMMAND", "ZED_PKG_PARSE_ERRORS", "ZED_PKG_UNKNOWN_OPTIONS"];

// ... existing file content omitted in this replacement request is not safe ...
