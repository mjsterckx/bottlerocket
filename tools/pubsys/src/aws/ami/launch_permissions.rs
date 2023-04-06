use aws_sdk_ec2::Client as Ec2Client;
use snafu::ResultExt;

/// Returns the launch permissions for the given AMI
pub(crate) async fn get_launch_permissions(
    ec2_client: &Ec2Client,
    region: &str,
    ami_id: &str,
) -> Result<Vec<LaunchPermissionDef>> {
    let ec2_response = ec2_client
        .describe_image_attribute()
        .image_id(ami_id)
        .attribute(aws_sdk_ec2::model::ImageAttributeName::LaunchPermission)
        .send()
        .await
        .context(error::DescribeImageAttributeSnafu {
            ami_id,
            region: region.to_string(),
        })?;

    Ok(ec2_response
        .launch_permissions()
        .unwrap_or(&[])
        .iter()
        .cloned()
        .map(LaunchPermissionDef::from)
        .collect())
}

mod error {
    use aws_sdk_ec2::error::DescribeImageAttributeError;
    use aws_sdk_ec2::types::SdkError;
    use snafu::Snafu;

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub(super)))]
    pub(crate) enum Error {
        #[snafu(display("Error describing AMI {} in {}: {}", ami_id, region, source))]
        DescribeImageAttribute {
            ami_id: String,
            region: String,
            #[snafu(source(from(SdkError<DescribeImageAttributeError>, Box::new)))]
            source: Box<SdkError<DescribeImageAttributeError>>,
        },
    }
}
pub(crate) use error::Error;

use super::LaunchPermissionDef;
type Result<T> = std::result::Result<T, error::Error>;
