{{- define "crypto-collector.name" -}}
{{- .Chart.Name | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "crypto-collector.fullname" -}}
{{- printf "%s" (include "crypto-collector.name" .) | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "crypto-collector.labels" -}}
helm.sh/chart: {{ .Chart.Name }}-{{ .Chart.Version }}
app.kubernetes.io/name: {{ include "crypto-collector.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "crypto-collector.selectorLabels" -}}
app.kubernetes.io/name: {{ include "crypto-collector.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}
