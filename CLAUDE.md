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
`429 Too Many Requests ... toomanyrequests: Data limit exceeded`. The workflow
publishes only the GitHub release archive; it never pushes to ECR. `publish`
exits 0 when `snapbot-<sha>` already exists.

## Deploy (arm64 only, route66 GH #4470)

The snapbot Lambdas are arm64-only. The ECR repository `route66-snapbot` has
IMMUTABLE tags, and the image is built only by the Graviton CodeBuild project
`route66-snapbot-arm64-image`. Never build or push a snapbot image from the
laptop: a workstation build is amd64, the stack refuses it, and the immutable
SHA tag it occupies can never be replaced by the correct arm64 build.

The only supported command, from the route66 checkout against clean, pushed
snapbot main:

```
python D:/git/route66/scripts/deployment/deploy_snapbot.py deploy
```

With no `--sha` it selects HEAD; if HEAD has no ECR image, `attestor/deploy.py`
builds it on CodeBuild first, refuses any image that is not arm64, and deploys
the stack by immutable digest. `--sha <full sha>` only selects an earlier image
that CodeBuild already published (for a later deployment-tool-only commit); it
never builds.
