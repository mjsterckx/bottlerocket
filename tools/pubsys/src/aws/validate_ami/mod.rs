//! The validate_ami module owns the 'validate-ami' subcommand and controls the process of validating
//! EC2 images

pub(crate) mod ami;
pub(crate) mod results;

use self::ami::ImageDef;
use self::results::{AmiValidationResult, AmiValidationResultStatus, AmiValidationResults};
use crate::aws::client::build_client_config;
use crate::aws::validate_ami::ami::describe_images;
use crate::Args;
use aws_sdk_ec2::{Client as AmiClient, Region};
use log::{info, trace};
use pubsys_config::InfraConfig;
use snafu::ResultExt;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::PathBuf;
use structopt::{clap, StructOpt};

/// Validates EC2 images
#[derive(Debug, StructOpt)]
#[structopt(setting = clap::AppSettings::DeriveDisplayOrder)]
pub(crate) struct ValidateAmiArgs {
    /// File holding the expected amis
    #[structopt(long, parse(from_os_str))]
    expected_amis_path: PathBuf,

    /// Optional path where the validation results should be written
    #[structopt(long, parse(from_os_str))]
    write_results_path: Option<PathBuf>,

    #[structopt(long, requires = "write-results-path")]
    /// Optional filter to only write validation results with these statuses to the above path
    /// The available statuses are: `Correct`, `Incorrect`, `Missing`
    write_results_filter: Option<Vec<AmiValidationResultStatus>>,

    #[structopt(long)]
    /// If this argument is given, print the validation results summary as a JSON object instead
    /// of a plaintext table
    json: bool,
}

/// Performs EC2 image validation and returns the `AmiValidationResults` object
pub(crate) async fn validate(
    args: &Args,
    validate_ami_args: &ValidateAmiArgs,
) -> Result<AmiValidationResults> {
    info!("Parsing Infra.toml file");

    // If a lock file exists, use that, otherwise use Infra.toml
    let infra_config = InfraConfig::from_path_or_lock(&args.infra_config_path, false)
        .context(error::ConfigSnafu)?;

    let aws = infra_config.aws.clone().unwrap_or_default();

    trace!("Parsed infra config: {:#?}", infra_config);

    // Parse the expected ami file
    info!("Parsing expected ami file");
    let expected_images = parse_expected_amis(&validate_ami_args.expected_amis_path).await?;

    info!("Parsed expected ami file");

    // Create a HashMap of AmiClients, one for each region where validation should happen
    let base_region = &Region::new(aws.regions[0].clone());
    let mut ami_clients = HashMap::with_capacity(expected_images.len());

    for region in expected_images.keys() {
        let client_config = build_client_config(region, base_region, &aws).await;
        let ami_client = AmiClient::new(&client_config);
        ami_clients.insert(region.clone(), ami_client);
    }

    // Create a map of image_id to a bool indicating whether or not the image is public. This map
    // is needed to determine if launch permissions should be retrieved for the image (which is the
    // case if it is not public)
    let expected_image_public = expected_images
        .iter()
        .flat_map(|(_, expected_images)| {
            expected_images
                .into_iter()
                .map(move |image_def| (image_def.id.clone(), image_def.public))
        })
        .collect::<HashMap<String, bool>>();

    // Retrieve the EC2 images using the AmiClients
    info!("Retrieving EC2 images");
    let images = describe_images(
        &ami_clients,
        &expected_images
            .iter()
            .map(|(region, images)| {
                (
                    region.clone(),
                    images.iter().map(|i| i.id.clone()).collect::<Vec<String>>(),
                )
            })
            .collect::<HashMap<Region, Vec<String>>>(),
        &expected_image_public,
    )
    .await;

    // Validate the retrieved EC2 images per region
    info!("Validating EC2 images");
    let results: HashMap<Region, ami::Result<HashSet<AmiValidationResult>>> = images
        .into_iter()
        .map(|(region, region_result)| {
            (
                region.clone(),
                region_result.map(|result| {
                    validate_images_in_region(
                        expected_images
                            .get(region)
                            .map(|e| e.to_owned())
                            .unwrap_or(vec![]),
                        &result,
                        region,
                    )
                }),
            )
        })
        .collect::<HashMap<Region, ami::Result<HashSet<AmiValidationResult>>>>();

    let validation_results = AmiValidationResults::new(results);

    // If a path was given to write the results to, write the results
    if let Some(write_results_path) = &validate_ami_args.write_results_path {
        // Filter the results by given status, and if no statuses were given, get all results
        info!("Writing results to file");
        let filtered_results = validation_results.get_results_for_status(
            validate_ami_args
                .write_results_filter
                .as_ref()
                .unwrap_or(&vec![
                    AmiValidationResultStatus::Correct,
                    AmiValidationResultStatus::Incorrect,
                    AmiValidationResultStatus::Missing,
                ]),
        );

        // Write the results as JSON
        serde_json::to_writer_pretty(
            &File::create(write_results_path).context(error::WriteValidationResultsSnafu {
                path: write_results_path,
            })?,
            &filtered_results,
        )
        .context(error::SerializeValidationResultsSnafu)?;
    }

    Ok(validation_results)
}

/// Validates EC2 images in a single region, based on a Vec (ImageDef) of expected images
/// and a HashMap (AmiId, ImageDef) of actual retrieved images. Returns a HashSet of
/// AmiValidationResult objects.
pub(crate) fn validate_images_in_region(
    expected_images: Vec<ImageDef>,
    actual_images: &HashMap<AmiId, ImageDef>,
    region: &Region,
) -> HashSet<AmiValidationResult> {
    let mut results = HashSet::new();

    // Validate all expected images, creating an AmiValidationResult object
    for mut image in expected_images {
        // If the image is expected to be public, the specific launch permissions are irrelevant
        // and were not retrieved for the actual image
        if image.public {
            image.launch_permissions = None;
        }
        results.insert(AmiValidationResult::new(
            image.id.clone(),
            image.clone(),
            actual_images.get(&image.id).map(|v| v.to_owned()),
            region.clone(),
        ));
    }

    results
}

type RegionName = String;
type AmiId = String;

/// Parse the file holding image values. Return a HashMap of Region mapped to a vec of ImageDefs
/// for that region.
pub(crate) async fn parse_expected_amis(
    expected_amis_path: &PathBuf,
) -> Result<HashMap<Region, Vec<ImageDef>>> {
    // Parse the JSON file as a HashMap of region_name, mapped to a JSON value
    let expected_amis: HashMap<RegionName, serde_json::Value> = serde_json::from_reader(
        &File::open(expected_amis_path.clone()).context(error::ReadExpectedImagesFileSnafu {
            path: expected_amis_path,
        })?,
    )
    .context(error::ParseExpectedImagesFileSnafu)?;

    // If the value for each Region is a JSON object instead of an array, wrap it in an array
    let vectored_images = expected_amis
        .into_iter()
        .map(|(region, value)| {
            let image_values = match value.as_array() {
                Some(array) => array.to_owned(),
                None => vec![value],
            };
            (Region::new(region), image_values)
        })
        .collect::<HashMap<Region, Vec<serde_json::Value>>>();

    // Parse the arrays of JSON objects into vecs of ImageDefs
    let image_map = vectored_images
        .into_iter()
        .map(|(region, image_values)| {
            let images = image_values
                .into_iter()
                .map(serde_json::from_value::<ImageDef>)
                .collect::<serde_json::Result<Vec<ImageDef>>>();
            match images {
                Ok(images) => Ok((region, images)),
                Err(e) => Err(e),
            }
        })
        .collect::<serde_json::Result<HashMap<Region, Vec<ImageDef>>>>()
        .context(error::ParseExpectedImagesFileSnafu)?;

    Ok(image_map)
}

/// Common entrypoint from main()
pub(crate) async fn run(args: &Args, validate_ami_args: &ValidateAmiArgs) -> Result<()> {
    let results = validate(args, validate_ami_args).await?;

    if validate_ami_args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&results.get_json_summary())
                .context(error::SerializeResultsSummarySnafu)?
        )
    } else {
        println!("{}", results);
    }
    Ok(())
}

mod error {
    use snafu::Snafu;
    use std::path::PathBuf;

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub(super)))]
    pub(crate) enum Error {
        #[snafu(display("Error reading config: {}", source))]
        Config { source: pubsys_config::Error },

        #[snafu(display("Error reading validation config at path {:?}: {}", path, source))]
        ReadValidationConfig {
            source: std::io::Error,
            path: PathBuf,
        },

        #[snafu(display("Error parsing validation config: {}", source))]
        ParseValidationConfig { source: serde_json::Error },

        #[snafu(display(
            "Failed to get launch permissions for image {} in region {}: {}",
            image_id,
            region,
            source
        ))]
        LauchPermissions {
            image_id: String,
            region: String,
            source: crate::aws::ami::launch_permissions::Error,
        },

        #[snafu(display("Missing field in validation config: {}", missing))]
        MissingField { missing: String },

        #[snafu(display("Infra.toml is missing {}", missing))]
        MissingConfig { missing: String },

        #[snafu(display("Failed to parse image file: {}", source))]
        ParseExpectedImagesFile { source: serde_json::Error },

        #[snafu(display("Failed to read image file: {:?}", path))]
        ReadExpectedImagesFile {
            source: std::io::Error,
            path: PathBuf,
        },

        #[snafu(display("Invalid validation status filter: {}", filter))]
        InvalidStatusFilter { filter: String },

        #[snafu(display("Failed to serialize validation results to json: {}", source))]
        SerializeValidationResults { source: serde_json::Error },

        #[snafu(display("Failed to write validation results to {:?}: {}", path, source))]
        WriteValidationResults {
            path: PathBuf,
            source: std::io::Error,
        },

        #[snafu(display("Failed to serialize results summary to JSON: {}", source))]
        SerializeResultsSummary { source: serde_json::Error },
    }
}

pub(crate) use error::Error;

type Result<T> = std::result::Result<T, error::Error>;

#[cfg(test)]
mod test {
    use super::ami::ImageDef;
    use super::validate_images_in_region;
    use crate::aws::{
        ami::LaunchPermissionDef,
        validate_ami::results::{AmiValidationResult, AmiValidationResultStatus},
    };
    use aws_sdk_ec2::Region;
    use std::collections::{HashMap, HashSet};

    // These tests assert that the images can be validated correctly.

    // Tests validation of images where the expected value is equal to the actual value
    #[test]
    fn validate_images_all_correct() {
        let expected_parameters: Vec<ImageDef> = vec![
            ImageDef {
                id: "test1-image-id".to_string(),
                name: "test1-image".to_string(),
                public: true,
                launch_permissions: None,
                ena_support: true,
                sriov_net_support: "simple".to_string(),
            },
            ImageDef {
                id: "test2-image-id".to_string(),
                name: "test2-image".to_string(),
                public: true,
                launch_permissions: None,
                ena_support: true,
                sriov_net_support: "simple".to_string(),
            },
            ImageDef {
                id: "test3-image-id".to_string(),
                name: "test3-image".to_string(),
                public: true,
                launch_permissions: None,
                ena_support: true,
                sriov_net_support: "simple".to_string(),
            },
        ];
        let actual_parameters: HashMap<String, ImageDef> = HashMap::from([
            (
                "test1-image-id".to_string(),
                ImageDef {
                    id: "test1-image-id".to_string(),
                    name: "test1-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                },
            ),
            (
                "test2-image-id".to_string(),
                ImageDef {
                    id: "test2-image-id".to_string(),
                    name: "test2-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                },
            ),
            (
                "test3-image-id".to_string(),
                ImageDef {
                    id: "test3-image-id".to_string(),
                    name: "test3-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                },
            ),
        ]);
        let expected_results = HashSet::from_iter(vec![
            AmiValidationResult::new(
                "test3-image-id".to_string(),
                ImageDef {
                    id: "test3-image-id".to_string(),
                    name: "test3-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                },
                Some(ImageDef {
                    id: "test3-image-id".to_string(),
                    name: "test3-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                }),
                Region::new("us-west-2"),
            ),
            AmiValidationResult::new(
                "test2-image-id".to_string(),
                ImageDef {
                    id: "test2-image-id".to_string(),
                    name: "test2-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                },
                Some(ImageDef {
                    id: "test2-image-id".to_string(),
                    name: "test2-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                }),
                Region::new("us-west-2"),
            ),
            AmiValidationResult::new(
                "test1-image-id".to_string(),
                ImageDef {
                    id: "test1-image-id".to_string(),
                    name: "test1-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                },
                Some(ImageDef {
                    id: "test1-image-id".to_string(),
                    name: "test1-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                }),
                Region::new("us-west-2"),
            ),
        ]);
        let results = validate_images_in_region(
            expected_parameters,
            &actual_parameters,
            &Region::new("us-west-2"),
        );

        for result in &results {
            assert_eq!(result.status, AmiValidationResultStatus::Correct);
        }
        assert_eq!(results, expected_results);
    }

    // Tests validation of images where the expected value is different from the actual value
    #[test]
    fn validate_images_all_incorrect() {
        let expected_parameters: Vec<ImageDef> = vec![
            ImageDef {
                id: "test1-image-id".to_string(),
                name: "test1-image".to_string(),
                public: true,
                launch_permissions: None,
                ena_support: true,
                sriov_net_support: "simple".to_string(),
            },
            ImageDef {
                id: "test2-image-id".to_string(),
                name: "test2-image".to_string(),
                public: true,
                launch_permissions: None,
                ena_support: true,
                sriov_net_support: "simple".to_string(),
            },
            ImageDef {
                id: "test3-image-id".to_string(),
                name: "test3-image".to_string(),
                public: true,
                launch_permissions: None,
                ena_support: true,
                sriov_net_support: "simple".to_string(),
            },
        ];
        let actual_parameters: HashMap<String, ImageDef> = HashMap::from([
            (
                "test1-image-id".to_string(),
                ImageDef {
                    id: "test1-image-id".to_string(),
                    name: "test1-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: false,
                    sriov_net_support: "simple".to_string(),
                },
            ),
            (
                "test2-image-id".to_string(),
                ImageDef {
                    id: "test2-image-id".to_string(),
                    name: "test2-image".to_string(),
                    public: false,
                    launch_permissions: Some(vec![LaunchPermissionDef {
                        group: Some("all".to_string()),
                        user_id: None,
                        organization_arn: None,
                        organizational_unit_arn: None,
                    }]),
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                },
            ),
            (
                "test3-image-id".to_string(),
                ImageDef {
                    id: "test3-image-id".to_string(),
                    name: "test3-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: true,
                    sriov_net_support: "not simple".to_string(),
                },
            ),
        ]);
        let expected_results = HashSet::from_iter(vec![
            AmiValidationResult::new(
                "test3-image-id".to_string(),
                ImageDef {
                    id: "test3-image-id".to_string(),
                    name: "test3-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                },
                Some(ImageDef {
                    id: "test3-image-id".to_string(),
                    name: "test3-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: true,
                    sriov_net_support: "not simple".to_string(),
                }),
                Region::new("us-west-2"),
            ),
            AmiValidationResult::new(
                "test2-image-id".to_string(),
                ImageDef {
                    id: "test2-image-id".to_string(),
                    name: "test2-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                },
                Some(ImageDef {
                    id: "test2-image-id".to_string(),
                    name: "test2-image".to_string(),
                    public: false,
                    launch_permissions: Some(vec![LaunchPermissionDef {
                        group: Some("all".to_string()),
                        user_id: None,
                        organization_arn: None,
                        organizational_unit_arn: None,
                    }]),
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                }),
                Region::new("us-west-2"),
            ),
            AmiValidationResult::new(
                "test1-image-id".to_string(),
                ImageDef {
                    id: "test1-image-id".to_string(),
                    name: "test1-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                },
                Some(ImageDef {
                    id: "test1-image-id".to_string(),
                    name: "test1-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: false,
                    sriov_net_support: "simple".to_string(),
                }),
                Region::new("us-west-2"),
            ),
        ]);
        let results = validate_images_in_region(
            expected_parameters,
            &actual_parameters,
            &Region::new("us-west-2"),
        );
        for result in &results {
            assert_eq!(result.status, AmiValidationResultStatus::Incorrect);
        }
        assert_eq!(results, expected_results);
    }

    // Tests validation of images where the actual value is missing
    #[test]
    fn validate_images_all_missing() {
        let expected_parameters: Vec<ImageDef> = vec![
            ImageDef {
                id: "test1-image-id".to_string(),
                name: "test1-image".to_string(),
                public: true,
                launch_permissions: None,
                ena_support: true,
                sriov_net_support: "simple".to_string(),
            },
            ImageDef {
                id: "test2-image-id".to_string(),
                name: "test2-image".to_string(),
                public: true,
                launch_permissions: None,
                ena_support: true,
                sriov_net_support: "simple".to_string(),
            },
            ImageDef {
                id: "test3-image-id".to_string(),
                name: "test3-image".to_string(),
                public: true,
                launch_permissions: None,
                ena_support: true,
                sriov_net_support: "simple".to_string(),
            },
        ];
        let actual_parameters = HashMap::new();
        let expected_results = HashSet::from_iter(vec![
            AmiValidationResult::new(
                "test3-image-id".to_string(),
                ImageDef {
                    id: "test3-image-id".to_string(),
                    name: "test3-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                },
                None,
                Region::new("us-west-2"),
            ),
            AmiValidationResult::new(
                "test2-image-id".to_string(),
                ImageDef {
                    id: "test2-image-id".to_string(),
                    name: "test2-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                },
                None,
                Region::new("us-west-2"),
            ),
            AmiValidationResult::new(
                "test1-image-id".to_string(),
                ImageDef {
                    id: "test1-image-id".to_string(),
                    name: "test1-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                },
                None,
                Region::new("us-west-2"),
            ),
        ]);
        let results = validate_images_in_region(
            expected_parameters,
            &actual_parameters,
            &Region::new("us-west-2"),
        );
        for result in &results {
            assert_eq!(result.status, AmiValidationResultStatus::Missing);
        }
        assert_eq!(results, expected_results);
    }

    // Tests validation of parameters where each status (Correct, Incorrect, Missing) happens once
    #[test]
    fn validate_images_mixed() {
        let expected_parameters: Vec<ImageDef> = vec![
            ImageDef {
                id: "test1-image-id".to_string(),
                name: "test1-image".to_string(),
                public: true,
                launch_permissions: None,
                ena_support: true,
                sriov_net_support: "simple".to_string(),
            },
            ImageDef {
                id: "test2-image-id".to_string(),
                name: "test2-image".to_string(),
                public: true,
                launch_permissions: None,
                ena_support: true,
                sriov_net_support: "simple".to_string(),
            },
            ImageDef {
                id: "test3-image-id".to_string(),
                name: "test3-image".to_string(),
                public: true,
                launch_permissions: None,
                ena_support: true,
                sriov_net_support: "simple".to_string(),
            },
        ];
        let actual_parameters: HashMap<String, ImageDef> = HashMap::from([
            (
                "test1-image-id".to_string(),
                ImageDef {
                    id: "test1-image-id".to_string(),
                    name: "test1-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                },
            ),
            (
                "test2-image-id".to_string(),
                ImageDef {
                    id: "test2-image-id".to_string(),
                    name: "test2-image".to_string(),
                    public: false,
                    launch_permissions: Some(vec![LaunchPermissionDef {
                        group: Some("all".to_string()),
                        user_id: None,
                        organization_arn: None,
                        organizational_unit_arn: None,
                    }]),
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                },
            ),
        ]);
        let expected_results = HashSet::from_iter(vec![
            AmiValidationResult::new(
                "test1-image-id".to_string(),
                ImageDef {
                    id: "test1-image-id".to_string(),
                    name: "test1-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                },
                Some(ImageDef {
                    id: "test1-image-id".to_string(),
                    name: "test1-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                }),
                Region::new("us-west-2"),
            ),
            AmiValidationResult::new(
                "test2-image-id".to_string(),
                ImageDef {
                    id: "test2-image-id".to_string(),
                    name: "test2-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                },
                Some(ImageDef {
                    id: "test2-image-id".to_string(),
                    name: "test2-image".to_string(),
                    public: false,
                    launch_permissions: Some(vec![LaunchPermissionDef {
                        group: Some("all".to_string()),
                        user_id: None,
                        organization_arn: None,
                        organizational_unit_arn: None,
                    }]),
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                }),
                Region::new("us-west-2"),
            ),
            AmiValidationResult::new(
                "test3-image-id".to_string(),
                ImageDef {
                    id: "test3-image-id".to_string(),
                    name: "test3-image".to_string(),
                    public: true,
                    launch_permissions: None,
                    ena_support: true,
                    sriov_net_support: "simple".to_string(),
                },
                None,
                Region::new("us-west-2"),
            ),
        ]);
        let results = validate_images_in_region(
            expected_parameters,
            &actual_parameters,
            &Region::new("us-west-2"),
        );

        assert_eq!(results, expected_results);
    }
}
