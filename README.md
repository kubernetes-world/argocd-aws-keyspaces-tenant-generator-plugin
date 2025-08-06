# argocd-aws-keyspaces-tenant-generator-plugin

Argo CD ApplicationSet **plugin generator** that reads tenant configuration from **Amazon Keyspaces** (Cassandra compatible) and returns one parameter map per tenant. Built in Rust with Axum and the `scylla` driver using `rustls`.

> Result: one Argo CD **Application** per tenant, rendered from your repo using values fetched from Keyspaces.

---

## Features
- Simple HTTP plugin that implements the ApplicationSet **Plugin Generator** contract at `POST /api/v1/getparams.execute`.
- Reads tenants from a Keyspaces table and returns an array of parameter maps.
- `scylla 1.x` client with `rustls` TLS. No OpenSSL on the runtime image.
- Bearer token check for ApplicationSet to plugin calls.
- Optional label filtering via generator input. Example: only tenants with `region=ca-central-1`.
- Minimal container and Kubernetes manifests. Works with IRSA if you choose to fetch secrets from AWS Secrets Manager.

## Architecture overview
```
ApplicationSet ──HTTP──> keyspaces-tenant-gen (this service)
                            │
                            └── TLS ──> Amazon Keyspaces (Cassandra)
```
- ApplicationSet posts a request to the plugin with a bearer token.
- The plugin queries the `tenant_configs` table in Keyspaces.
- The plugin returns `{"output":{"parameters":[...]}}`. ApplicationSet templates one `Application` per parameter map.

## Requirements
- Rust 1.85 or newer. Tested with 1.89.
- Amazon Keyspaces with a keyspace and table as shown below.
- Service specific credentials for Keyspaces (username and password) or an alternative credential flow of your choice.
- The Starfield Class 2 Root certificate mounted in the container (PEM).

## Supported versions
- `scylla = "1.3.x"` with feature `rustls-023`.
- `axum = "0.8.x"`, `tokio = "1.x"`.

---

## Quick start

### 1) Create the Keyspaces schema (MVP)
```sql
CREATE KEYSPACE IF NOT EXISTS tenant_ops
WITH REPLICATION = {'class': 'SingleRegionStrategy'};

CREATE TABLE IF NOT EXISTS tenant_ops.tenant_configs (
  tenant_id       text,
  enabled         boolean,
  namespace       text,
  target_cluster  text,
  repo_url        text,
  repo_path       text,
  labels          map<text, text>,
  params          map<text, text>,
  PRIMARY KEY ((tenant_id))
);

INSERT INTO tenant_ops.tenant_configs (tenant_id, enabled, namespace, target_cluster, repo_url, repo_path, labels, params)
VALUES ('acme', true, 'tn-acme', 'in-cluster', 'https://github.com/yourorg/tenants.git', 'tenants/acme',
        {'tier':'gold','region':'ca-central-1'}, {'kafkaTopic':'acme.events','mskCluster':'primary'});
```

### 2) Build the binary and the image
```bash
# build
cargo build --release

# optional: build image
docker build -t ghcr.io/<ORG>/argocd-aws-keyspaces-tenant-generator-plugin:0.0.1 .
```

### 3) Kubernetes manifests (excerpt)
ServiceAccount with IRSA and Deployment that mounts the token and CA file.
```yaml
apiVersion: v1
kind: ServiceAccount
metadata:
  name: keyspaces-plugin
  namespace: argocd
  annotations:
    eks.amazonaws.com/role-arn: arn:aws:iam::<ACCOUNT_ID>:role/argocd-keyspaces-plugin
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: keyspaces-plugin
  namespace: argocd
spec:
  replicas: 1
  selector:
    matchLabels: { app: keyspaces-plugin }
  template:
    metadata:
      labels: { app: keyspaces-plugin }
    spec:
      serviceAccountName: keyspaces-plugin
      containers:
        - name: plugin
          image: ghcr.io/<ORG>/argocd-aws-keyspaces-tenant-generator-plugin:0.0.1
          ports:
            - containerPort: 4355
          env:
            - name: AWS_REGION
              value: ca-central-1
            - name: KEYSPACES_ROOT_CERT
              value: /certs/sf-class2-root.crt
            - name: PLUGIN_TOKEN_FILE
              value: /var/run/argo/token
            - name: KEYSPACES_USERNAME
              valueFrom:
                secretKeyRef:
                  name: keyspaces-auth
                  key: username
            - name: KEYSPACES_PASSWORD
              valueFrom:
                secretKeyRef:
                  name: keyspaces-auth
                  key: password
          volumeMounts:
            - name: plugin-token
              mountPath: /var/run/argo
              readOnly: true
            - name: keyspaces-root-ca
              mountPath: /certs
              readOnly: true
      volumes:
        - name: plugin-token
          secret:
            secretName: argocd-secret
            items:
              - key: plugin.keyspaces.token
                path: token
        - name: keyspaces-root-ca
          configMap:
            name: keyspaces-root-ca
---
apiVersion: v1
kind: Service
metadata:
  name: keyspaces-plugin
  namespace: argocd
spec:
  selector:
    app: keyspaces-plugin
  ports:
    - name: http
      port: 80
      targetPort: 4355
```

Starfield CA as a ConfigMap:
```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: keyspaces-root-ca
  namespace: argocd
data:
  sf-class2-root.crt: |
    -----BEGIN CERTIFICATE-----
    (paste Starfield Class 2 Root here)
    -----END CERTIFICATE-----
```

Token Secret in `argocd-secret`:
```yaml
apiVersion: v1
kind: Secret
metadata:
  name: argocd-secret
  namespace: argocd
stringData:
  plugin.keyspaces.token: "please-change-me"
```

### 4) ApplicationSet plugin configuration and usage
Config for the generator plugin:
```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: as-plugin-keyspaces
  namespace: argocd
  labels:
    app.kubernetes.io/part-of: argocd
data:
  token: "$argocd-secret:plugin.keyspaces.token"
  baseUrl: "http://keyspaces-plugin.argocd.svc.cluster.local"
  requestTimeout: "30"
```

ApplicationSet that creates one Application per tenant:
```yaml
apiVersion: argoproj.io/v1alpha1
kind: ApplicationSet
metadata:
  name: tenants-from-keyspaces
  namespace: argocd
spec:
  goTemplate: true
  goTemplateOptions: ["missingkey=error"]
  generators:
    - plugin:
        configMapRef:
          name: as-plugin-keyspaces
        input:
          parameters:
            filterLabelKey: region
            filterLabelValue: ca-central-1
        requeueAfterSeconds: 120
  template:
    metadata:
      name: "tn-{{ .tenantId }}"
      labels:
        tenant: "{{ .tenantId }}"
        region: "{{ index .labels "region" }}"
        tier: "{{ index .labels "tier" }}"
    spec:
      project: default
      destination:
        name: "{{ .cluster }}"       # in-cluster or a named cluster
        namespace: "{{ .namespace }}"
      source:
        repoURL: "{{ .repoURL }}"
        targetRevision: HEAD
        path: "{{ .path }}"
        helm:
          values: |
            tenantId: {{ .tenantId }}
            kafka:
              topic: {{ .kafkaTopic }}
              cluster: {{ .mskCluster }}
      syncPolicy:
        automated:
          prune: true
          selfHeal: true
        syncOptions:
          - CreateNamespace=true
```

### Request and response shape
Request from ApplicationSet to the plugin:
```json
{
  "applicationSetName": "tenants-from-keyspaces",
  "input": {
    "parameters": {
      "filterLabelKey": "region",
      "filterLabelValue": "ca-central-1"
    }
  }
}
```

Response from the plugin:
```json
{
  "output": {
    "parameters": [
      {
        "tenantId": "acme",
        "namespace": "tn-acme",
        "cluster": "in-cluster",
        "repoURL": "https://github.com/yourorg/tenants.git",
        "path": "tenants/acme",
        "labels": {"tier": "gold", "region": "ca-central-1"},
        "kafkaTopic": "acme.events",
        "mskCluster": "primary",
        "params": {"kafkaTopic": "acme.events", "mskCluster": "primary"}
      }
    ]
  }
}
```

---

## Configuration
Environment variables used by the service:

| Variable | Default | Notes |
|---|---|---|
| `PORT` | `4355` | HTTP listener port |
| `AWS_REGION` | `us-east-1` | Region for the Keyspaces endpoint hostname |
| `PLUGIN_TOKEN_FILE` | `/var/run/argo/token` | File that contains the bearer token for plugin calls |
| `KEYSPACES_ROOT_CERT` | `/certs/sf-class2-root.crt` | Path to Starfield Class 2 Root certificate (PEM) |
| `KEYSPACES_USERNAME` | none | Service specific username from Keyspaces |
| `KEYSPACES_PASSWORD` | none | Service specific password from Keyspaces |

> Optional: fetch credentials from AWS Secrets Manager using IRSA. In that case the service would look up a secret ID and parse a JSON payload into username and password.

---

## Security notes
- The HTTP endpoint checks a bearer token from `argocd-secret`. Keep this secret scoped to Argo CD.
- Use IRSA and fine grained IAM policies if you integrate with AWS Secrets Manager or other AWS APIs.
- Scope Keyspaces permissions to the exact keyspace and table. Allow `cassandra:Select` and `cassandra:Describe` at minimum for reads.
- Always load the Starfield CA and connect to `cassandra.<region>.amazonaws.com:9142` with TLS enabled.

## Production tips
- For very large tenant sets, consider a table that makes scanning efficient without `ALLOW FILTERING`. For example `active_tenants(bucket text, tenant_id text, ...)` and scan a handful of buckets.
- Add metrics for query latency, rows returned, and error counts. Expose Prometheus metrics.
- Use `requeueAfterSeconds` in the ApplicationSet generator to control polling cadence.

## Troubleshooting
- `401/403` from the plugin: verify the bearer token value in `argocd-secret` and that it is mounted to the container at `PLUGIN_TOKEN_FILE`.
- TLS errors: confirm the Starfield CA file is present and that the path matches `KEYSPACES_ROOT_CERT`.
- Authentication to Keyspaces fails: verify that credentials are service specific, not IAM credentials.
- Build errors about `scylla` methods: ensure you are on `scylla 1.3.x` and using `query_unpaged(...).into_rows_result()?.rows::<T>()?` or the iterator API.

## Versioning and releases
- Conventional Commits are encouraged. Example: `feat(plugin): initial Keyspaces-backed generator`.
- Create signed tags. Example: `v0.0.1`.

## License
Apache License 2.0. See [LICENSE](LICENSE).

## Contributing
Issues and PRs are welcome. Please discuss large changes in an issue first. Keep the code free of unnecessary dependencies and aim for small container images.

