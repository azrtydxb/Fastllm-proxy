{{- define "fastllm.name" -}}{{ default .Chart.Name .Values.nameOverride }}{{- end -}}
{{- define "fastllm.fullname" -}}
{{- printf "%s-%s" .Release.Name (include "fastllm.name" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- define "fastllm.labels" -}}
app.kubernetes.io/name: {{ include "fastllm.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}
{{- define "fastllm.image" -}}
{{ .Values.image.repository }}:{{ .Values.image.tag | default .Chart.AppVersion }}
{{- end -}}
{{- define "fastllm.secretName" -}}
{{ .Values.secrets.existingSecret | default (printf "%s-secrets" (include "fastllm.fullname" .)) }}
{{- end -}}
