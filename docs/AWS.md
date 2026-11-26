# Deploying Chuck on AWS

## Read this first

Chuck has no real authentication yet. Locally it treats every request as one user and binds to loopback. The only built-in
protection is an optional shared secret (`CHUCK_TOKEN`, bearer token), which the browser UI does not send, so it protects
the API only for scripted clients.

**Do not expose Chuck to a network until you add an `Authenticator` for your identity provider** (see below).
Putting it behind an authenticating ALB without that step still leaves every user sharing the single `local` owner.

## Recommended shape

    Internet/VPN -> ALB (HTTPS, OIDC/Cognito auth action) -> ECS Fargate task (1 replica) -> EBS or RDS

- **Container:** build with the included `Dockerfile`, push to ECR. The container listens on `0.0.0.0:8080`, runs as a
  non-root user, and keeps SQLite at `/data/chuck.db`.
- **Health check:** the ALB target group should check `GET /healthz` (returns `ok`).
- **TLS:** terminate on the ALB with an ACM certificate. Keep the task in private subnets; the task security group should
  accept port 8080 only from the ALB security group.
- **Auth:** configure the ALB listener rule with an `authenticate-oidc` or `authenticate-cognito` action in front of the
  forward action. The ALB then adds signed `x-amzn-oidc-*` headers to each request.

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

1. **ALB authenticator.** Implement `auth::Authenticator` that verifies the `x-amzn-oidc-data` JWT (ES256, public key
   fetched from `https://public-keys.auth.elb.<region>.amazonaws.com/<kid>`, check `signer` equals your ALB ARN and
   `exp`) and returns the `sub` claim as the owner id. Select it in `main.rs` by environment variable. Every store call is
   already scoped by owner.
2. **Reject requests that bypass the ALB.** Lock the task security group to the ALB, so nobody can send forged headers directly.
3. **Backups.** EBS snapshot schedule (Data Lifecycle Manager), or RDS automated backups.
4. **Reminders.** They only fire while a browser tab is open. Server-side delivery (SES email or SNS) needs a scheduler,
   for example a loop in the service or EventBridge Scheduler calling an internal endpoint.

## Build and run locally in a container

    docker build -t chuck .
    docker run --rm -p 127.0.0.1:8080:8080 -v chuck-data:/data chuck

Then open http://127.0.0.1:8080.

## Push to ECR (example)

    aws ecr create-repository --repository-name chuck --profile <profile>
    aws ecr get-login-password --profile <profile> | docker login --username AWS --password-stdin <acct>.dkr.ecr.<region>.amazonaws.com
    docker build --platform linux/amd64 -t <acct>.dkr.ecr.<region>.amazonaws.com/chuck:latest .
    docker push <acct>.dkr.ecr.<region>.amazonaws.com/chuck:latest

Use `--platform linux/arm64` instead if the task definition uses Graviton.
