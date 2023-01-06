// Copyright 2019-2022 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use base64::{engine::general_purpose, Engine as _};
use crate::{EnclaveBuildError, Result};
use futures::stream::StreamExt;
use log::{debug, info};
use serde_json::{json, Value};
use shiplift::RegistryAuth;
use shiplift::{BuildOptions, Docker, PullOptions};
use std::{fs::File, io::Write, path::Path};
use tempfile::NamedTempFile;
use tokio::runtime::Runtime;
use url::Url;

/// Docker inspect architecture constants
pub const DOCKER_ARCH_ARM64: &str = "arm64";
pub const DOCKER_ARCH_AMD64: &str = "amd64";

/// Struct exposing the Docker functionalities to the EIF builder
pub struct DockerUtil {
    docker: Docker,
    docker_image: String,
}

impl DockerUtil {
    /// Constructor that takes as argument a tag for the docker image to be used
    pub fn new(docker_image: String) -> Self {
        let mut docker_image = docker_image;

        if !docker_image.contains(':') {
            docker_image.push_str(":latest");
        }

        DockerUtil {
            // DOCKER_HOST environment variable is parsed inside
            // if docker daemon address needs to be substituted.
            // By default it tries to connect to 'unix:///var/run/docker.sock'
            docker: Docker::new(),
            docker_image,
        }
    }

    /// Returns the credentials by reading ${HOME}/.docker/config.json or ${DOCKER_CONFIG}
    ///
    /// config.json doesn't seem to have a schema that we could use to validate
    /// we are parsing it correctly, so the parsing mechanism had been infered by
    /// reading a config.json created by:
    //         Docker version 19.03.2
    fn get_credentials(&self) -> Result<RegistryAuth> {
        let image = self.docker_image.clone();
        let host = if let Ok(uri) = Url::parse(&image) {
            uri.host().map(|s| s.to_string())
        } else {
            // Some Docker URIs don't have the protocol included, so just use
            // a dummy one to trick Url that it's a properly defined Uri.
            let uri = format!("dummy://{image}");
            if let Ok(uri) = Url::parse(&uri) {
                uri.host().map(|s| s.to_string())
            } else {
                None
            }
        };

        if let Some(registry_domain) = host {
            let config_file = self.get_config_file()?;

            let config_json: serde_json::Value =
                serde_json::from_reader(&config_file).map_err(|err| {
                    EnclaveBuildError::CredentialsError(format!(
                        "JSON was not well-formatted: {err}",
                    ))
                })?;

            let auths = config_json.get("auths").ok_or_else(|| {
                EnclaveBuildError::CredentialsError(
                    "Could not find auths key in config JSON".to_string(),
                )
            })?;

            if let Value::Object(auths) = auths {
                for (registry_name, registry_auths) in auths.iter() {
                    if !registry_name.to_string().contains(&registry_domain) {
                        continue;
                    }

                    let auth = registry_auths
                        .get("auth")
                        .ok_or_else(|| {
                            EnclaveBuildError::CredentialsError(
                                "Could not find auth key in config JSON".to_string(),
                            )
                        })?
                        .to_string();

                    let auth = auth.replace('"', "");
                    let decoded = general_purpose::STANDARD.decode(&auth).map_err(|err| {
                        EnclaveBuildError::CredentialsError(format!(
                            "Invalid Base64 encoding for auth: {err}",
                        ))
                    })?;
                    let decoded = std::str::from_utf8(&decoded).map_err(|err| {
                        EnclaveBuildError::CredentialsError(format!(
                            "Invalid utf8 encoding for auth: {err}",
                        ))
                    })?;

                    if let Some(index) = decoded.rfind(':') {
                        let (user, after_user) = decoded.split_at(index);
                        let (_, password) = after_user.split_at(1);
                        return Ok(RegistryAuth::builder()
                            .username(user)
                            .password(password)
                            .build());
                    }
                }
            }
        }

        Err(EnclaveBuildError::CredentialsError(
            "No credentials found for the current image".to_string(),
        ))
    }

    fn get_config_file(&self) -> Result<File> {
        if let Ok(file) = std::env::var("DOCKER_CONFIG") {
            let config_file = File::open(file).map_err(|err| {
                EnclaveBuildError::CredentialsError(format!(
                    "Could not open file pointed by env\
                     DOCKER_CONFIG: {err}"
                ))
            })?;
            Ok(config_file)
        } else {
            if let Ok(home_dir) = std::env::var("HOME") {
                let default_config_path = format!("{home_dir}/.docker/config.json");
                let config_path = Path::new(&default_config_path);
                if config_path.exists() {
                    let config_file = File::open(config_path).map_err(|err| {
                        EnclaveBuildError::CredentialsError(format!(
                            "Could not open file {:?}: {}",
                            config_path.to_str(),
                            err
                        ))
                    })?;
                    return Ok(config_file);
                }
            }
            Err(EnclaveBuildError::CredentialsError(
                "Config file not present, please set env \
                 DOCKER_CONFIG accordingly"
                    .to_string(),
            ))
        }
    }

    /// Pull the image, with the tag provided in constructor, from the Docker registry
    pub fn pull_image(&self) -> Result<()> {
        let act = async {
            // Check if the Docker image is locally available.
            // If available, early exit.
            if self
                .docker
                .images()
                .get(&self.docker_image)
                .inspect()
                .await
                .is_ok()
            {
                eprintln!("Using the locally available Docker image...");
                return Ok(());
            }

            let mut pull_options_builder = PullOptions::builder();
            pull_options_builder.image(&self.docker_image);

            match self.get_credentials() {
                Ok(auth) => {
                    pull_options_builder.auth(auth);
                }
                // It is not mandatory to have the credentials set, but this is
                // the most likely reason for failure when pulling, so log the
                // error.
                Err(err) => {
                    debug!("WARNING!! Credential could not be set {:?}", err);
                }
            };

            let mut stream = self.docker.images().pull(&pull_options_builder.build());

            loop {
                if let Some(item) = stream.next().await {
                    match item {
                        Ok(output) => {
                            let msg = &output;

                            if let Some(err_msg) = msg.get("error") {
                                break Err(EnclaveBuildError::ImagePullError(err_msg.to_string()));
                            } else {
                                info!("{}", msg);
                            }
                        }
                        Err(e) => {
                            break Err(EnclaveBuildError::ImagePullError(e.to_string()));
                        }
                    }
                } else {
                    break Ok(());
                }
            }
        };

        let runtime = Runtime::new().map_err(|_| EnclaveBuildError::RuntimeError)?;

        runtime.block_on(act)
    }

    /// Build an image locally, with the tag provided in constructor, using a
    /// directory that contains a Dockerfile
    pub fn build_image(&self, dockerfile_dir: String) -> Result<()> {
        let act = async {
            let mut stream = self.docker.images().build(
                &BuildOptions::builder(dockerfile_dir)
                    .tag(self.docker_image.clone())
                    .build(),
            );

            loop {
                if let Some(item) = stream.next().await {
                    match item {
                        Ok(output) => {
                            let msg = &output;

                            if let Some(err_msg) = msg.get("error") {
                                break Err(EnclaveBuildError::ImageBuildError(err_msg.to_string()));
                            } else {
                                info!("{}", msg);
                            }
                        }
                        Err(e) => {
                            break Err(EnclaveBuildError::ImageBuildError(e.to_string()));
                        }
                    }
                } else {
                    break Ok(());
                }
            }
        };

        let runtime = Runtime::new().map_err(|_| EnclaveBuildError::RuntimeError)?;

        runtime.block_on(act)
    }

    /// Inspect docker image and return its description as a json String
    pub fn inspect_image(&self) -> Result<serde_json::Value> {
        let act = async {
            match self.docker.images().get(&self.docker_image).inspect().await {
                Ok(image) => Ok(json!(image)),
                Err(e) => Err(EnclaveBuildError::ImageInspectError(e)),
            }
        };

        let runtime = Runtime::new().map_err(|_| EnclaveBuildError::RuntimeError)?;
        runtime.block_on(act)
    }

    fn extract_image(&self) -> Result<(Vec<String>, Vec<String>)> {
        // First try to find CMD parameters (together with potential ENV bindings)
        let act_cmd = async {
            match self.docker.images().get(&self.docker_image).inspect().await {
                Ok(image) => image.config.cmd.ok_or(EnclaveBuildError::EntrypointError),
                Err(e) => Err(EnclaveBuildError::ImageInspectError(e)),
            }
        };
        let act_env = async {
            match self.docker.images().get(&self.docker_image).inspect().await {
                Ok(image) => image.config.env.ok_or(EnclaveBuildError::EntrypointError),
                Err(e) => Err(EnclaveBuildError::ImageInspectError(e)),
            }
        };

        let check_cmd_runtime = Runtime::new()
            .map_err(|_| EnclaveBuildError::RuntimeError)?
            .block_on(act_cmd);
        let check_env_runtime = Runtime::new()
            .map_err(|_| EnclaveBuildError::RuntimeError)?
            .block_on(act_env);

        // If no CMD instructions are found, try to locate an ENTRYPOINT command
        if check_cmd_runtime.is_err() || check_env_runtime.is_err() {
            let act_entrypoint = async {
                match self.docker.images().get(&self.docker_image).inspect().await {
                    Ok(image) => image
                        .config
                        .entrypoint
                        .ok_or(EnclaveBuildError::EntrypointError),
                    Err(e) => Err(EnclaveBuildError::ImageInspectError(e)),
                }
            };

            let check_entrypoint_runtime = Runtime::new()
                .map_err(|_| EnclaveBuildError::RuntimeError)?
                .block_on(act_entrypoint);

            if check_entrypoint_runtime.is_err() {
                return Err(EnclaveBuildError::EntrypointError);
            }

            let act = async {
                match self.docker.images().get(&self.docker_image).inspect().await {
                    Ok(image) => Ok((
                        image.config.entrypoint.unwrap(),
                        image.config.env.ok_or_else(Vec::<String>::new).unwrap(),
                    )),
                    Err(e) => Err(EnclaveBuildError::ImageInspectError(e)),
                }
            };

            let runtime = Runtime::new().map_err(|_| EnclaveBuildError::RuntimeError)?;

            return runtime.block_on(act);
        }

        let act = async {
            match self.docker.images().get(&self.docker_image).inspect().await {
                Ok(image) => Ok((image.config.cmd.unwrap(), image.config.env.unwrap())),
                Err(e) => Err(EnclaveBuildError::ImageInspectError(e)),
            }
        };

        let runtime = Runtime::new().map_err(|_| EnclaveBuildError::RuntimeError)?;

        runtime.block_on(act)
    }

    /// The main function of this struct. This needs to be called in order to
    /// extract the necessary configuration values from the docker image with
    /// the tag provided in the constructor
    pub fn load(&self) -> Result<(NamedTempFile, NamedTempFile)> {
        let (cmd, env) = self.extract_image()?;

        let cmd_file = write_config(cmd)?;
        let env_file = write_config(env)?;

        Ok((cmd_file, env_file))
    }

    /// Fetch architecture information from an image
    pub fn architecture(&self) -> Result<String> {
        let arch = async {
            match self.docker.images().get(&self.docker_image).inspect().await {
                Ok(image) => Ok(image.architecture),
                Err(e) => Err(EnclaveBuildError::ImageInspectError(e)),
            }
        };

        let runtime = Runtime::new().map_err(|_| EnclaveBuildError::RuntimeError)?;

        runtime.block_on(arch)
    }
}

pub fn write_config(config: Vec<String>) -> Result<NamedTempFile> {
    let mut file = NamedTempFile::new().map_err(|_| EnclaveBuildError::ConfigError)?;

    for line in config {
        file.write_fmt(format_args!("{line}\n"))
            .map_err(|_| EnclaveBuildError::ConfigError)?;
    }

    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// Test extracted configuration is as expected
    #[test]
    fn test_config() {
        #[cfg(target_arch = "x86_64")]
        let docker = DockerUtil::new(String::from(
            "667861386598.dkr.ecr.us-east-1.amazonaws.com/enclaves-samples:vsock-sample-server-x86_64",
        ));
        #[cfg(target_arch = "aarch64")]
        let docker = DockerUtil::new(String::from(
            "667861386598.dkr.ecr.us-east-1.amazonaws.com/enclaves-samples:vsock-sample-server-aarch64",
        ));

        let (cmd_file, env_file) = docker.load().unwrap();
        let mut cmd_file = File::open(cmd_file.path()).unwrap();
        let mut env_file = File::open(env_file.path()).unwrap();

        let mut cmd = String::new();
        cmd_file.read_to_string(&mut cmd).unwrap();
        assert_eq!(
            cmd,
            "/bin/sh\n\
             -c\n\
             ./vsock-sample server --port 5005\n"
        );

        let mut env = String::new();
        env_file.read_to_string(&mut env).unwrap();
        assert_eq!(
            env,
            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n"
        );
    }
}
