As the Builder, implement item #{{.Number}} — {{.Title}}.

{{- if .Revision}}

## Revision request

Apply the feedback below, re-run relevant checks, and update report.json.
{{.Revision}}
{{- end}}

## Item body

{{.Body}}
