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

{{/* Shared ORION_* environment for the server and migrate containers. */}}
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
{{- with .Values.extraEnv }}
{{ toYaml . }}
{{- end }}
{{- end }}
