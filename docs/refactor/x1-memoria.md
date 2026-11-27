# X1 — Prioridade de VRAM para buffers e janelas

O experimento mede se o FPS renderizado sobe quando os dados lidos pelos shaders ganham espaço na VRAM às custas de novas texturas. A comparação usa dois processos novos da mesma build.

## Switches

| Variável | Efeito |
| --- | --- |
| `VICTORY_RF_X1_STATS=1` | Ativa o log `rf-x1:` com a política de memória normal. |
| `VICTORY_RF_X1_PRIORITY=1` | Ativa a reserva conjunta e o log. Os dois modos ficam desligados por padrão. |
| `VICTORY_RF_X1_RESERVE_MB` | Tamanho da reserva conjunta em MiB, com padrão de `3072`. `0` zera o espaço do experimento para novas alocações locais de `lookup` e `loose`. |

## Alterações

- `shadow.rs` em `card_room_kept_on` e `card_room_kept` faz o switch de prioridade usar o mecanismo de `VICTORY_CARD_ROOM_FOR_BUFFERS` com a reserva conjunta. Uma recusa deixa a reserva disponível para as próximas alocações.
- `shadow.rs` em `card_for`, `window_card_for` e `priority_card_for` usa a mesma reserva para os buffers e as janelas, por ordem de alocação. O caminho passa pelos recursos locais existentes, com `Card::staging` na RAM. A reserva substitui os limites de capacidade e o mínimo de tamanho da política normal. O orçamento absoluto do DXGI e o limite técnico de 1 GiB por buffer do título continuam valendo. Os caminhos opcionais `wc_mapped_for` e `window_mapped_for` também debitam a reserva quando as cópias em VRAM estão habilitadas. `VICTORY_CARD_BUFFERS=0` desativa a reserva.
- `resource.rs` em `room`, `RoomWeights::new` e `log_room` soma a parte ainda livre dessa reserva à cobrança da fila de texturas, junto com a reserva existente calculada por `targets_room_kept`. A cobrança vale desde o início, inclusive abaixo do piso de texturas e junto com um teto explícito de texturas. Sem amostra DXGI, o saldo da reserva reduz o limite de contingência de 4 GiB. Texturas novas aguardam na fila. Texturas carregadas seguem o ciclo de vida normal.
- `resource.rs` em `hosted` registra o tamanho guest da descrição completa, com todos os mips, pela mesma função `footprint` usada pelo runtime. O tamanho host usa `GetResourceAllocationInfo` via `host_bytes`, mais os tiles com backing alocado. `host.rs` em `Meter::frame` chama `rf_x1.rs::log` junto ao log da fila, a cada cerca de cinco segundos.

## Como ler o log

- `windows` e `buffers` mostram quantidade/MiB dos recursos que os shaders veem, por destino de alocação. Staging fica fora desses totais. `buffers` inclui as cópias auxiliares de views de `exact`, que continuam na RAM. As arenas de `by_address` e os recursos abertos diretamente sobre a memória do título ficam fora do censo.
- `refused` conta recusas de capacidade no caminho de cópia local. `alloc_failed` conta falhas da chamada de alocação nesse caminho. Os dois são acumulados desde o início. `retried` conta apenas tentativas de promoção na mesma vida do recurso e fica em zero nesta build.
- `textures vram` soma as texturas do host, incluindo alvos, twins e tiles com backing alocado. `sampled` mostra a parte preenchida pelo título. `guest` e `host` comparam os footprints das texturas primárias com tamanho guest conhecido. O host exclui os twins auxiliares dessa comparação. `unknown_guest` mostra quantas texturas primárias ficaram fora do pareamento. O tamanho guest inclui a cadeia completa mesmo para texturas com alguns mips mantidos no host. Nesses casos, o total mede o custo alocado no host para a população carregada, e a diferença inclui o efeito dos mips ausentes além de alinhamento e formato.
- `reserve` mostra o usado/total da reserva por cópias principais, arredondado para MiB no log. `vram_in_use`, `vram_budget` e `shared_with_card_in_use` vêm do DXGI. Se `dxgi_available=false`, os campos DXGI aparecem como zero. A classificação local do censo mede o destino pedido para os recursos. O WDDM pode paginar parte do conjunto para a RAM compartilhada.

## Riscos e nova tentativa

`lookup` e `loose` publicam um `Shadow` com um recurso e GPUVA fixos. Uma lista já gravada pode ter essa GPUVA embutida, enquanto um descritor persistente pode manter o `ID3D12Resource` anterior e voltar a ser usado depois de um fence. Uma área com escrita de shader também pode ter estado mais recente associado à cópia antiga. A promoção segura exigiria rastrear e atualizar esses consumidores além de sincronizar a última gravação. O item 1c fica fora deste experimento. Uma janela descartada normalmente por `let_go_idle` disputa a reserva se o título voltar a pedi-la como nova alocação.

Com 3 GiB reservados, a fila pode trazer menos detalhes de textura, especialmente durante o carregamento. Uma cópia local também mantém seu staging e pode demandar memória auxiliar de escrita de shaders fora da reserva. O orçamento DXGI é uma amostra e pode mudar durante a execução. O ganho depende tanto da quantidade de dados frequentes que migra quanto do custo de atualizar as novas cópias locais.

## Teste unitário no Windows

```powershell
cargo test --locked -p victory-gpu --lib rf_x1
```

Os testes cobrem o teto conjunto, a cobrança do saldo na fila, o teto absoluto do DXGI, a contabilidade viva e uma nova alocação depois da liberação da reserva. A execução dos testes fica com o orquestrador.

## Roteiro A/B

Use a build release deste branch, o mesmo perfil Series X e as mesmas configurações nas duas execuções. Substitua `$gameExe` pelo caminho do executável do seu dump. Chegue ao mesmo ponto da cena pesada a partir do mesmo save, repita o percurso e compare uma janela de 60 segundos depois do carregamento. Encerre a primeira execução antes da segunda.

```powershell
# A — política normal com telemetria
.\scripts\play-nvidia.ps1 -Exe $gameExe -More @{VICTORY_RF_X1_STATS='1'; VICTORY_RF_X1_PRIORITY='0'; VICTORY_LOG='info'}

# B — prioridade de VRAM com a reserva padrão
.\scripts\play-nvidia.ps1 -Exe $gameExe -More @{VICTORY_RF_X1_STATS='0'; VICTORY_RF_X1_PRIORITY='1'; VICTORY_RF_X1_RESERVE_MB='3072'; VICTORY_LOG='info'}
```

Compare o FPS renderizado, o painel F3 e os `rf-x1:` nos logs em `%LOCALAPPDATA%\victory\logs`. Observe o aumento de `windows vram` e `buffers vram`, a ocupação da reserva, a queda ou estabilidade de `textures sampled`, as recusas e a RAM compartilhada. Confira se elementos da cena continuam visíveis. Se o censo local pouco mudar entre A e B, a rodada não isolou o efeito da residência sobre o FPS.
