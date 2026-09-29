# route66-snapbot

## ECR

The CI snapbot Lambda `CommandCenterSnapbot-app` runs the image
`760773574016.dkr.ecr.us-east-1.amazonaws.com/route66-snapbot:<full commit sha>`,
a private repository in the command-center account (boto3 profile
`command-center`, region `us-east-1`). The GitHub release archive
`snapbot-<sha>` serves local Kumo only. Lambda never reads it. A published
release with no ECR image leaves the Lambda on its old image.

Every base image comes from ECR Public (`public.ecr.aws/docker/library/golang`,
`public.ecr.aws/lambda/provided:al2023`). Owner, verbatim: "authenticate on ECR,
do NOT move to docker hub". A `FROM` pointing at Docker Hub is rejected.

`.github/workflows/image.yml` pulls ECR Public anonymously and has no AWS
credential. Run 36532827638 (commit f3b4946) failed its `test` job with
`429 Too Many Requests ... toomanyrequests: Data limit exceeded`, and no release
published. The repository has no Actions secrets, and no AWS account holds a
GitHub OIDC provider or a role trusting this repository. The workflow cannot
publish until an owner-authorized credential exists: an OIDC role scoped to
`repo:redzilla-org/route66-snapbot:*` with `ecr-public:GetAuthorizationToken`
and `sts:GetServiceBearerToken`, then `aws-actions/configure-aws-credentials`
and `aws-actions/amazon-ecr-login` (`registry-type: public`) ahead of both
docker builds.

Until then an image is published from the owner workstation, from the route66
checkout, against a clean snapbot checkout at the commit:

```
python D:/git/route66/scripts/maintenance/publish_snapbot_release_from_laptop_gh4115.py D:/git/route66-snapbot <sha>
python D:/git/route66/scripts/maintenance/push_snapbot_image_to_ecr_gh4115.py <sha>
```

The first builds `ghcr.io/redzilla-org/route66-snapbot:<sha>` locally and
creates the `snapbot-<sha>` release. The second tags that local image into the
ECR repository, pushes it, and prints the ECR digest. f3b4946 was published this
way (`sha256:d522f0f4eae89af8e92951a266bf33193c9b649fd6a6432ad3d78489af44de1d`).
The Lambda moves to a new image through route66
`scripts/deployment/deploy_snapbot.py`.
