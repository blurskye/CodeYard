{{- define "codeyard.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 35 | trimSuffix "-" -}}
{{- end -}}

{{- define "codeyard.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 35 | trimSuffix "-" -}}
{{- else if contains .Chart.Name .Release.Name -}}
{{- .Release.Name | trunc 35 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name (include "codeyard.name" .) | trunc 35 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "codeyard.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | quote }}
app.kubernetes.io/name: {{ include "codeyard.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "codeyard.selectorLabels" -}}
app.kubernetes.io/name: {{ include "codeyard.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "codeyard.image" -}}
{{- if .Values.image.digest -}}
{{- printf "%s@%s" .Values.image.repository .Values.image.digest -}}
{{- else -}}
{{- printf "%s:%s" .Values.image.repository (default .Chart.AppVersion .Values.image.tag) -}}
{{- end -}}
{{- end -}}

{{- define "codeyard.secretName" -}}
{{- default (include "codeyard.fullname" .) .Values.secrets.existingSecret -}}
{{- end -}}

{{- define "codeyard.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "codeyard.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}
