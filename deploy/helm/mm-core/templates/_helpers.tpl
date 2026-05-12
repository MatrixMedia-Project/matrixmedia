{{- define "mm-core.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "mm-core.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "mm-core.labels" -}}
app.kubernetes.io/name: {{ include "mm-core.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" }}
{{- end -}}

{{- define "mm-core.selectorLabels" -}}
app.kubernetes.io/name: {{ include "mm-core.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- /*
Shared envFrom for both init (migrate) and main (serve) containers.
Pulls non-secret config from the chart's ConfigMap and MM_*-named keys
from the appConfig Secret as-is.
*/ -}}
{{- define "mm-core.envFrom" -}}
- configMapRef:
    name: {{ include "mm-core.fullname" . }}
- secretRef:
    name: {{ .Values.secrets.appConfig }}
{{- end -}}

{{- /*
Shared env: block — explicit mappings where the platform's Secret keys
don't already match the MM_* names mm-core expects.
*/ -}}
{{- define "mm-core.envMappings" -}}
- name: MM_DATABASE_URL
  valueFrom:
    secretKeyRef:
      name: {{ .Values.secrets.postgres }}
      key: DATABASE_URL
- name: MM_STORAGE_S3_ENDPOINT
  valueFrom:
    secretKeyRef:
      name: {{ .Values.secrets.s3 }}
      key: S3_ENDPOINT
- name: MM_STORAGE_S3_BUCKET
  valueFrom:
    secretKeyRef:
      name: {{ .Values.secrets.s3 }}
      key: S3_BUCKET
- name: MM_STORAGE_S3_REGION
  valueFrom:
    secretKeyRef:
      name: {{ .Values.secrets.s3 }}
      key: S3_REGION
- name: MM_STORAGE_S3_ACCESS_KEY
  valueFrom:
    secretKeyRef:
      name: {{ .Values.secrets.s3 }}
      key: S3_ACCESS_KEY_ID
- name: MM_STORAGE_S3_SECRET_KEY
  valueFrom:
    secretKeyRef:
      name: {{ .Values.secrets.s3 }}
      key: S3_SECRET_ACCESS_KEY
- name: MM_STORAGE_S3_PATH_STYLE
  valueFrom:
    secretKeyRef:
      name: {{ .Values.secrets.s3 }}
      key: S3_PATH_STYLE
{{- end -}}
