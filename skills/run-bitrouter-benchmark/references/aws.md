# Operating BitRouter OSS on AWS

This is an operational method for an agent acting as the AWS operator. It is
not a Terraform, CloudFormation, Docker Compose, systemd, or deployment-script
specification. Adapt the same identity, network, secret, health, tagging, cost,
and cleanup principles to other environments without pretending this document
defines their APIs.

## Ask the human for authority, not secrets

Ask for:

- AWS profile name or assume-role path;
- expected account, region, and permitted network source;
- create-new versus reuse-existing intent;
- instance/network/storage constraints and cleanup preference;
- secret-service choice or pre-existing secret references by name.

Never request raw AWS access keys in chat. Do not infer permission from a
profile's existence. Before mutation, run the selected identity's
`aws sts get-caller-identity`, show account and principal, present the resource
and approximate cost plan, and obtain the single resolved-plan confirmation.

## Keep three identities separate

### Deployment operator

This identity creates or inspects the BitRouter host and its network resources.
Derive least privilege from the confirmed operations. Typical capability
families may include EC2 describe/create/tag/terminate actions and the specific
SSM or secret reads chosen by the user; do not present one static policy as
universally correct.

### BitRouter instance role

An instance role is optional. Attach one only for confirmed host capabilities
such as SSM management, reading named secrets, writing logs, or calling an AWS
model provider. Scope resources and regions. The deployment operator needs
`iam:PassRole` only when it actually attaches this role.

### Harbor controller

This identity is needed only if Harbor itself uses an AWS/EC2 environment. It
is independent from the BitRouter deployment operator and instance role. Use
the installed Harbor EC2 environment documentation to derive its calls and
resource scope. Give trial sandboxes no IAM role unless the benchmark and human
explicitly require one.

If Harbor uses local Docker or another environment while BitRouter runs on AWS,
do not request Harbor EC2 permissions.

## Prove both SSH paths

When Harbor runs from an EC2 controller, treat these as two independent paths:

1. **Operator to controller.** Determine the source address from the actual SSH
   transport. An HTTP or SOCKS proxy's observed egress is not proof of the
   direct TCP source, and mobile or consumer networks may change it.
2. **Controller to sandbox.** Determine the address Harbor actually selects for
   SSH and the source address AWS will see on that path. Do not reuse the
   operator's CIDR for this rule.

Choose the sandbox address mode from observed network facts:

| Harbor SSH target | Sandbox bootstrap egress | Required SSH proof |
|---|---|---|
| Public IP | Public IPv4 assignment plus an Internet Gateway route | A security-group reference to the controller does not authorize traffic sent to the sandbox public IP. Use a confirmed, temporary controller public-egress `/32` rule and prove controller-to-sandbox SSH. |
| Private IP | NAT, required VPC endpoints, or an image that needs no external bootstrap downloads | A controller security-group reference can be used when both private interfaces and VPC routing permit it; prove private SSH and every bootstrap dependency. |

Do not assume public and private address-selection flags from another Harbor
version. Read the pinned backend as described in [harbor.md](harbor.md). Record
every temporary rule ID, exact source, purpose/run tag, and creation time, then
revoke and independently re-query each rule during cleanup.

Temporary SSH rules are not governed by the deployment cleanup preference.
Always revoke and independently re-query them at the confirmed stopping point
and on failure or abort, even when the human retains instances, volumes, or
other deployment resources.

Prefer a stable bastion, VPN, SSM, or another approved stable operator path when
the operator's address drifts. Never widen SSH ingress silently. If the human
explicitly authorizes a broader emergency rule, treat it as a time-bounded
deviation: state the exposure, preserve its exact rule ID, and revoke it at the
confirmed stopping point.

## Operate in this order

1. **Inspect.** Confirm STS identity, region, quota, existing resources, and
   selected network without mutation.
2. **Plan.** Show instance shape, image/source provenance, disk, public/private
   reachability, security rules, auth/TLS method, secret references, tags,
   expected hourly cost, and cleanup behavior.
3. **Confirm.** Use the single resolved-plan confirmation. Reconfirm only when
   the account, principal, region, permissions, resource scope, or expected cost
   materially changes.
4. **Create or reuse.** Tag every created resource with a unique run/project
   identity. Record resource IDs. Do not adopt unrelated existing resources.
5. **Install and configure.** Pin the BitRouter release/commit. Preserve the
   selected official/custom config provenance and diff. Keep upstream provider
   secrets on the host or in the confirmed secret service.
6. **Secure.** Prefer private reachability. If public ingress is necessary, use
   TLS, BitRouter inbound authentication, and the narrow confirmed source CIDR.
   Never expose an unauthenticated starter config.
7. **Validate.** Check service health and run the confirmed agent-specific
   smoke from the same network path Harbor will use. For Harbor EC2, first run
   one disposable canary that proves instance launch, selected SSH address,
   authentication, sandbox bootstrap, agent/model reachability, and normal
   instance/volume cleanup before starting the scored job.
8. **Operate.** Monitor availability and spend without changing the frozen
   route config during a job.
9. **Clean up.** Always revoke and re-query recorded temporary access rules.
   On request, remove only the other recorded resources created for this
   deployment. Re-query by exact IDs/tags and report retained resources and
   continuing cost.

## Stop conditions

Stop before mutation when STS identity is unexpected, required permission or
quota is missing, source/template provenance is ambiguous, the network would be
broader than confirmed, authentication/TLS is absent for public exposure, or
the cost/resource plan changed. Do not widen IAM or security groups silently.
