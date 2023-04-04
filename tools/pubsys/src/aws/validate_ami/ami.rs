//! The ami module owns the describing of images in EC2.

use aws_sdk_ec2::model::Image;
use aws_sdk_ec2::{Client as Ec2Client, Region};
use futures::future::{join, ready};
use futures::stream::{FuturesUnordered, StreamExt};
use log::{info, trace};
use serde::{Deserialize, Serialize};
use snafu::ResultExt;
use std::collections::HashMap;

use crate::aws::ami::launch_permissions::get_launch_permissions;
use crate::aws::ami::LaunchPermissionDef;

/// Structure of the EC2 image fields that should be validated
#[derive(Serialize, Deserialize, PartialEq, Eq, Hash, Debug, Clone)]
pub(crate) struct ImageDef {
    /// The id of the EC2 image
    pub(crate) id: String,

    /// The name of the EC2 image
    pub(crate) name: String,

    /// Whether or not the EC2 image is public
    #[serde(default)]
    pub(crate) public: bool,

    /// The launch permissions for the EC2 image.
    pub(crate) launch_permissions: Option<Vec<LaunchPermissionDef>>,

    /// Whether or not the EC2 image supports Elastic Network Adapter
    #[serde(default = "default_ena_support")]
    pub(crate) ena_support: bool,

    /// The level of the EC2 image's Single Root I/O Virtualization support
    #[serde(default = "default_sriov_net_support")]
    pub(crate) sriov_net_support: String,
}

fn default_ena_support() -> bool {
    true
}

fn default_sriov_net_support() -> String {
    "simple".to_string()
}

impl From<Image> for ImageDef {
    fn from(image: Image) -> Self {
        Self {
            id: image.image_id().unwrap_or_default().to_string(),
            name: image.name().unwrap_or_default().to_string(),
            public: image.public().unwrap_or_default(),
            // Will be populated later if the image is not expected to be public. If it is expected
            // to be public, then individual launch permissions are irrelevant
            launch_permissions: None,
            ena_support: image.ena_support().unwrap_or_default(),
            sriov_net_support: image.sriov_net_support().unwrap_or_default().to_string(),
        }
    }
}

pub(crate) async fn describe_images<'a>(
    clients: &'a HashMap<Region, Ec2Client>,
    image_ids: &HashMap<Region, Vec<String>>,
    expected_image_public: &HashMap<String, bool>,
) -> HashMap<&'a Region, Result<HashMap<String, ImageDef>>> {
    // Build requests for images; we have to request with a regional client so we split them by
    // region
    let mut requests = Vec::with_capacity(clients.len());
    for region in clients.keys() {
        trace!("Requesting images in {}", region);
        let ec2_client: &Ec2Client = &clients[region];
        let get_future = describe_images_in_region(
            region,
            ec2_client,
            image_ids
                .get(region)
                .map(|i| i.to_owned())
                .unwrap_or(vec![]),
            expected_image_public,
        );

        requests.push(join(ready(region), get_future));
    }

    // Send requests in parallel and wait for responses, collecting results into a list.
    requests
        .into_iter()
        .collect::<FuturesUnordered<_>>()
        .collect()
        .await
}

/// Fetches all images in a single region
pub(crate) async fn describe_images_in_region(
    region: &Region,
    client: &Ec2Client,
    image_ids: Vec<String>,
    expected_image_public: &HashMap<String, bool>,
) -> Result<HashMap<String, ImageDef>> {
    info!("Retrieving images in {}", region.to_string());
    let mut images = HashMap::new();

    // Send the request
    let mut get_future = client
        .describe_images()
        .include_deprecated(true)
        .set_image_ids(Some(image_ids))
        .into_paginator()
        .send();

    // Iterate over the retrieved images
    while let Some(page) = get_future.next().await {
        let retrieved_images = page
            .context(error::DescribeImagesSnafu {
                region: region.to_string(),
            })?
            .images()
            .unwrap_or_default()
            .to_owned();
        for image in retrieved_images {
            // Insert a new key-value pair into the map, with the key containing image id
            // and the value containing the ImageDef object created from the image
            let image_id = image
                .image_id()
                .ok_or(error::Error::MissingField {
                    missing: "image_id".to_string(),
                })?
                .to_string();
            let expected_public = expected_image_public.get(&image_id).ok_or(
                error::Error::MissingExpectedPublic {
                    missing: image_id.clone(),
                },
            )?;
            // If the image is not expected to be public, retrieve the launch permissions
            trace!(
                "Retrieving launch permissions for {} in {}",
                image_id,
                region.as_ref()
            );
            let mut image_def = ImageDef::from(image.to_owned());
            if !*expected_public {
                image_def.launch_permissions = Some(
                    get_launch_permissions(client, region.as_ref(), &image_id)
                        .await
                        .context(error::GetLaunchPermissionsSnafu {
                            region: region.as_ref(),
                            image_id: image_id.clone(),
                        })?,
                );
            }
            images.insert(image_id, image_def);
        }
    }

    info!("Images in {} have been retrieved", region.to_string());
    Ok(images)
}

pub(crate) mod error {
    use aws_sdk_ec2::error::DescribeImagesError;
    use aws_sdk_ssm::types::SdkError;
    use snafu::Snafu;

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub(super)))]
    #[allow(clippy::large_enum_variant)]
    pub(crate) enum Error {
        #[snafu(display("Failed to describe images in {}: {}", region, source))]
        DescribeImages {
            region: String,
            source: SdkError<DescribeImagesError>,
        },

        #[snafu(display(
            "Failed to retrieve launch permissions for image {} in region {}: {}",
            image_id,
            region,
            source
        ))]
        GetLaunchPermissions {
            region: String,
            image_id: String,
            source: crate::aws::ami::launch_permissions::Error,
        },

        #[snafu(display("Missing field in image: {}", missing))]
        MissingField { missing: String },

        #[snafu(display("Missing image id in expected image publicity map: {}", missing))]
        MissingExpectedPublic { missing: String },
    }
}

pub(crate) type Result<T> = std::result::Result<T, error::Error>;
