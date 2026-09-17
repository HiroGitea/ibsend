{{/* chart 名，可被 nameOverride 覆盖 */}}
{{- define "ibsend.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/* 资源全名：release 名里已经含有 chart 名时不再重复 */}}
{{- define "ibsend.fullname" -}}
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

{{- define "ibsend.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
选择器标签。kubectl ibsend 按 app.kubernetes.io/name=ibsend 和
app.kubernetes.io/component=daemon 找 daemon Pod，改名时要一起改插件参数。
*/}}
{{- define "ibsend.selectorLabels" -}}
app.kubernetes.io/name: {{ include "ibsend.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: daemon
{{- end }}

{{- define "ibsend.labels" -}}
helm.sh/chart: {{ include "ibsend.chart" . }}
{{ include "ibsend.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "ibsend.image" -}}
{{- printf "%s:%s" .Values.image.repository (.Values.image.tag | default .Chart.AppVersion) }}
{{- end }}

{{/* 运行 ibsend 的 UID/GID，给 fixPermissions 的 chown 用 */}}
{{- define "ibsend.owner" -}}
{{- $ctx := .Values.podSecurityContext | default dict }}
{{- $uid := 10001 }}
{{- if hasKey $ctx "runAsUser" }}{{ $uid = $ctx.runAsUser }}{{ end }}
{{- $gid := $uid }}
{{- if hasKey $ctx "runAsGroup" }}{{ $gid = $ctx.runAsGroup }}{{ end }}
{{- printf "%d:%d" (int $uid) (int $gid) }}
{{- end }}
