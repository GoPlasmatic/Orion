{{/* Expand the name of the chart. */}}
{{- define "orion.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/* Fully qualified app name. */}}
{{- define "orion.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/* Chart label. */}}
{{- define "orion.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/* Common labels. */}}
{{- define "orion.labels" -}}
helm.sh/chart: {{ include "orion.chart" . }}
{{ include "orion.selectorLabels" . }}
app.kubernetes.io/version: {{ .Values.image.tag | default .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/* Selector labels. */}}
{{- define "orion.selectorLabels" -}}
app.kubernetes.io/name: {{ include "orion.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/* Service account name. */}}
{{- define "orion.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "orion.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/* Image reference. */}}
{{- define "orion.image" -}}
{{- printf "%s:%s" .Values.image.repository (.Values.image.tag | default .Chart.AppVersion) }}
{{- end }}

{{/* Effective storage URL (devStack overrides). */}}
{{- define "orion.storageUrl" -}}
{{- if .Values.devStack.enabled }}
{{- printf "postgres://%s:%s@%s-postgres:5432/%s" .Values.devStack.postgres.user .Values.devStack.postgres.password (include "orion.fullname" .) .Values.devStack.postgres.database }}
{{- else }}
{{- required "storage.url (or storage.existingSecret / devStack.enabled) is required" .Values.storage.url }}
{{- end }}
{{- end }}

{{/* Effective Redis URL (devStack overrides). */}}
{{- define "orion.redisUrl" -}}
{{- if .Values.devStack.enabled }}
{{- printf "redis://%s-redis:6379" (include "orion.fullname" .) }}
{{- else }}
{{- required "cluster.redisUrl is required when cluster.enabled" .Values.cluster.redisUrl }}
{{- end }}
{{- end }}

{{/* Name of the Secret carrying the storage URL. */}}
{{- define "orion.storageSecretName" -}}
{{- if .Values.storage.existingSecret }}
{{- .Values.storage.existingSecret }}
{{- else }}
{{- printf "%s-storage" (include "orion.fullname" .) }}
{{- end }}
{{- end }}

{{/* Name of the Secret the migrate Job reads the storage URL from: the
     operator-provided Secret when set (it exists independently of Helm),
     otherwise the hook-scoped copy rendered next to the Job — the regular
     chart-managed Secret is applied only AFTER pre-install hooks run. */}}
{{- define "orion.migrateStorageSecretName" -}}
{{- if .Values.storage.existingSecret }}
{{- .Values.storage.existingSecret }}
{{- else }}
{{- printf "%s-storage-migrate" (include "orion.fullname" .) }}
{{- end }}
{{- end }}

{{/* Effective runtime environment: devStack always runs as development. */}}
{{- define "orion.environment" -}}
{{- if .Values.devStack.enabled }}development{{- else }}{{ .Values.env }}{{- end }}
{{- end }}

{{/* Name of the Secret carrying the admin API keys. */}}
{{- define "orion.adminAuthSecretName" -}}
{{- if .Values.adminAuth.existingSecret }}
{{- .Values.adminAuth.existingSecret }}
{{- else }}
{{- printf "%s-admin-auth" (include "orion.fullname" .) }}
{{- end }}
{{- end }}

{{/* Address the dedicated metrics listener binds.

     Orion parses this as a literal SocketAddr and refuses a hostname, so the
     computed default is an IP. 0.0.0.0 rather than 127.0.0.1: the scraper is
     in another pod, and a loopback bind would be reachable only from inside
     this container. */}}
{{- define "orion.metricsBindAddr" -}}
{{- if .Values.metrics.bindAddr }}
{{- .Values.metrics.bindAddr }}
{{- else }}
{{- printf "0.0.0.0:%v" .Values.metrics.port }}
{{- end }}
{{- end }}

{{/* Shared ORION_* environment for the server and migrate containers.

     In practice only the Deployment includes this block; the migrate Job
     builds its own minimal env (storage URL alone), since `migrate` returns
     before observability, metrics or the listener are ever initialised. */}}
{{- define "orion.env" -}}
- name: ORION_STORAGE__URL
  valueFrom:
    secretKeyRef:
      name: {{ include "orion.storageSecretName" . }}
      key: storage-url
{{- /* devStack has no migrate Job (its DB is a release resource), so dev
       replicas migrate at boot; production replicas never do. */}}
- name: ORION_STORAGE__AUTO_MIGRATE
  value: {{ ternary "true" (.Values.storage.autoMigrate | toString) .Values.devStack.enabled | quote }}
{{- /* The root filesystem is read-only, so the default ./backups
       (= /app/backups) is unwritable; keep SQLite backups on the data
       volume, which is the persistent one when persistence is enabled. */}}
{{- if .Values.persistence.enabled }}
- name: ORION_STORAGE__BACKUP_DIR
  value: {{ printf "%s/backups" .Values.persistence.mountPath | quote }}
{{- end }}
{{- if .Values.cluster.enabled }}
- name: ORION_CLUSTER__ENABLED
  value: "true"
- name: ORION_CLUSTER__REDIS_URL
  value: {{ include "orion.redisUrl" . | quote }}
- name: ORION_CLUSTER__EPOCH_POLL_INTERVAL_MS
  value: {{ .Values.cluster.epochPollIntervalMs | quote }}
- name: ORION_CLUSTER__INSTANCE_ID
  valueFrom:
    fieldRef:
      fieldPath: metadata.name
{{- end }}
- name: ORION_SERVER__PORT
  value: {{ .Values.server.port | quote }}
- name: ORION_SERVER__SHUTDOWN_DRAIN_SECS
  value: {{ .Values.server.shutdownDrainSecs | quote }}
- name: ORION_SERVER__SHUTDOWN_FORCE_TIMEOUT_SECS
  value: {{ .Values.server.shutdownForceTimeoutSecs | quote }}
- name: ORION_LOGGING__FORMAT
  value: {{ .Values.logging.format | quote }}
- name: ORION_LOGGING__LEVEL
  value: {{ .Values.logging.level | quote }}
- name: ORION_METRICS__ENABLED
  value: {{ .Values.metrics.enabled | quote }}
{{- if .Values.metrics.enabled }}
{{- /* Two listeners on one port is a bind failure at best and, with
       SO_REUSEADDR on both sockets, a platform-dependent split of incoming
       connections at worst. Orion refuses it at boot; catching it at render
       time turns a CrashLoop into a failed `helm upgrade`. Only checkable
       here for the computed default — an explicit bindAddr is the operator's
       to own, and boot validation still covers it. */}}
{{- if and (not .Values.metrics.bindAddr) (eq (int .Values.metrics.port) (int .Values.server.port)) }}
{{- fail (printf "metrics.port (%v) must differ from server.port (%v): the dedicated metrics listener needs an address of its own. Set metrics.enabled=false to keep /metrics on the main listener behind admin auth." .Values.metrics.port .Values.server.port) }}
{{- end }}
- name: ORION_METRICS__BIND_ADDR
  value: {{ include "orion.metricsBindAddr" . | quote }}
{{- end }}
- name: ORION_ENVIRONMENT
  value: {{ include "orion.environment" . | quote }}
{{- if or .Values.adminAuth.existingSecret .Values.adminAuth.apiKeys }}
- name: ORION_ADMIN_AUTH__ENABLED
  value: {{ .Values.adminAuth.enabled | quote }}
- name: ORION_ADMIN_AUTH__API_KEYS
  valueFrom:
    secretKeyRef:
      name: {{ include "orion.adminAuthSecretName" . }}
      key: api-keys
{{- else if and .Values.adminAuth.enabled (not .Values.devStack.enabled) }}
{{- fail "adminAuth.existingSecret or adminAuth.apiKeys is required: the chart defaults to a production install with admin auth enforced. Set devStack.enabled=true for a throwaway dev install." }}
{{- end }}
- name: ORION_CORS__ALLOWED_ORIGINS
  value: {{ join "," .Values.cors.allowedOrigins | quote }}
{{- with .Values.extraEnv }}
{{ toYaml . }}
{{- end }}
{{- end }}
