# webo

Painel de saúde pro seu servidor — CPU, memória, disco, temperatura, bateria e
rede num painel só, leve de verdade (um binário Rust, < 50 MB de RAM), com API
JSON pronta pra automação e agentes (MCP).

```
docker compose -f deploy/docker-compose.yml up -d --build
# painel em http://seu-servidor:5050
```

## O que ele mostra

- **CPU** — uso, load, sparkline das últimas 24 h
- **Memória** — uso/total, sparkline
- **Disco** — uso/total/livre do filesystem raiz
- **Temperatura** — CPU, com alerta visual a 85 °C
- **Bateria** — carga, status e limite de carga (notebooks-servidor)
- **Rede** — taxas de download/upload
- **Sistema** — SO, kernel, arquitetura, processos, containers ativos

Cartões de métricas indisponíveis no seu hardware (bateria num datacenter,
por exemplo) somem sozinhos.

## API (o mesmo contrato que o painel usa)

| Endpoint | Retorna |
|---|---|
| `GET /api/v1/snapshot` | tudo do momento atual |
| `GET /api/v1/history?minutes=1440` | série de CPU/RAM/rede (amostra a cada 15 s, 24 h em memória) |
| `GET /api/v1/system` | identidade da máquina (hostname, SO, kernel, hardware) |
| `GET /healthz` | `ok` |

Nada é exclusivo da UI: tudo que o painel mostra sai desses endpoints — é por
eles que um servidor MCP (ou qualquer automação) enxerga a máquina.

## Configuração

| Env | Padrão | O quê |
|---|---|---|
| `WEBO_BIND` | `0.0.0.0:5050` | endereço de escuta |
| `WEBO_SAMPLE_SECS` | `15` | intervalo de coleta |
| `WEBO_NET_DEV` | `/proc/net/dev` | fonte das taxas de rede (em container, monte a do host) |

Os mounts do compose (`docker.sock`, `/sys`, `/proc/net/dev`) são opcionais —
sem eles o webo continua funcionando, só esconde as métricas correspondentes.

## Segurança

O webo **não tem autenticação própria** (é observador, read-only). Não exponha
a porta 5050 direto na internet: ponha na frente um proxy com auth,
Cloudflare Access, ou acesse por VPN/rede privada.

## Licença

MIT
