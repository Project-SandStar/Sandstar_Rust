# Alert System

## Overview
The alert system monitors channel values for fault conditions and sends notifications via configurable recipients.

## Configuration

Alerts are configured via the REST API or config file.

### Alert Config Structure
```json
{
  "enabled": true,
  "checkIntervalSecs": 30,
  "cooldownSecs": 300,
  "recipients": [
    {
      "type": "email",
      "address": "operator@example.com",
      "minSeverity": "warning"
    }
  ],
  "rules": [
    {
      "channel": 1113,
      "condition": "fault",
      "severity": "critical",
      "message": "Zone temperature sensor fault"
    }
  ]
}
```

## REST Endpoints

```bash
# Get alert configuration
curl http://localhost:8085/api/alerts/config

# Update alert configuration
curl -X PUT http://localhost:8085/api/alerts/config \
  -H 'Content-Type: application/json' \
  -d '{"enabled": true, "checkIntervalSecs": 30, ...}'

# Get active alerts
curl http://localhost:8085/api/alerts/active

# Get alert history
curl http://localhost:8085/api/alerts/history
```

## Alert Conditions

| Condition | Triggers When |
|-----------|---------------|
| `fault` | Channel status is "fault" |
| `down` | Channel status is "down" |
| `disabled` | Channel status is "disabled" |
| `value_high` | Value exceeds threshold |
| `value_low` | Value below threshold |
| `stale` | No value update within timeout |

## Severity Levels

| Level | Priority |
|-------|----------|
| `critical` | Immediate action required |
| `warning` | Attention needed |
| `info` | Informational |
