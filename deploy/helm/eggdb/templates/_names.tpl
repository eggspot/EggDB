{{- define "eggdb.name" -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- define "eggdb.secret" -}}
{{- default (include "eggdb.name" .) .Values.existingSecret -}}
{{- end -}}
