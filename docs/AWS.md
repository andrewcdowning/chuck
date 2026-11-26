# Deploying Chuck on AWS

## Read this first

Chuck refuses to listen on a non-loopback address unless authentication is configured. Pick a provider and set its
variables as described in [AUTH.md](AUTH.md). For an ALB with OIDC/Cognito, use the `jwt` provider with
`CHUCK_JWT_HEADER=x-amzn-oidc-data`; behind CloudFront with an edge function, use the `header` provider.

## Recommended shape

    Internet/VPN -> ALB (HTTPS, OIDC/Cognito auth action) -> ECS Fargate task (1 replica) -> EBS or RDS

- **Container:** build with the included `Dockerfile`, push to ECR. The container listens on `0.0.0.0:8080`, runs as a
  non-root user, and keeps SQLite at `/data/chuck.db`.
- **Health check:** the ALB target group should check `GET /healthz` (returns `ok`).
- **TLS:** terminate on the ALB with an ACM certificate. Keep the task in private subnets; the task security group should
  accept port 8080 only from the ALB security group.
- **Auth:** configure the ALB listener rule with an `authenticate-oidc` or `authenticate-cognito` action in front of the
  forward action. The ALB then adds signed `x-amzn-oidc-*` headers, which Chuck verifies (see [AUTH.md](AUTH.md)).

## Storage choices

| Option | Fits | Notes |
| --- | --- | --- |
| SQLite on an EBS volume, one EC2 instance or one ECS-on-EC2 task | A few users | Simplest. Single writer, so run exactly one replica. Back up with EBS snapshots. |
| SQLite on EFS | Not recommended | SQLite file locking over NFS is unreliable, and WAL mode does not work on network filesystems. |
| RDS/Aurora PostgreSQL | Many users, several replicas | Implement `NoteStore` for Postgres (full-text search via `tsvector`) and select it in `main.rs`. |
| DynamoDB | Many users | Implement `NoteStore`; search would need OpenSearch or a different approach, since DynamoDB has no full-text search. |

Fargate tasks have ephemeral disk. SQLite on Fargate loses data on restart unless the volume is persistent, so use EC2
with EBS, or move to RDS.

## What to build before going live

1. **Auth configuration.** Set the `CHUCK_AUTH` and `CHUCK_JWT_*` (or `CHUCK_AUTH_*`) variables in the task definition. Store secrets in Secrets Manager.
2. **Reject requests that bypass the ALB.** Lock the task security group to the ALB, so nobody can send forged headers directly.
3. **Backups.** EBS snapshot schedule (Data Lifecycle Manager), or RDS automated backups.
4. **Reminders.** They only fire while a browser tab is open. Server-side delivery (SES email or SNS) needs a scheduler,
   for example a loop in the service or EventBridge Scheduler calling an internal endpoint.

## Build and run locally in a container

    docker build -t chuck .
    docker run --rm -p 127.0.0.1:8080:8080 -e CHUCK_ALLOW_INSECURE=1 -v chuck-data:/data chuck

`CHUCK_ALLOW_INSECURE=1` is needed because the container listens on `0.0.0.0` inside Docker; publishing the port on 127.0.0.1 keeps it local. Then open http://127.0.0.1:8080.

## Push to ECR (example)

    aws ecr create-repository --repository-name chuck --profile <profile>
    aws ecr get-login-password --profile <profile> | docker login --username AWS --password-stdin <acct>.dkr.ecr.<region>.amazonaws.com
    docker build --platform linux/amd64 -t <acct>.dkr.ecr.<region>.amazonaws.com/chuck:latest .
    docker push <acct>.dkr.ecr.<region>.amazonaws.com/chuck:latest

Use `--platform linux/arm64` instead if the task definition uses Graviton.
