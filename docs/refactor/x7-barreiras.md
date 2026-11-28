# X7 — Barreiras GPU no fluxo do título

## Referência da captura

A referência é `C:\Users\mathe\orca\nsight-captures\gputrace-wfi\BASE`. Na RTX 4070, em Series S com X4 desligado, os dois quadros exportados duraram 62,89 e 60,53 ms na GPU. O FE esteve em Wait-For-Idle por 45–47% do tempo, com utilização dos SMs de cerca de 9%. Cerca de 81% do WFI atribuído aos intervalos-folha está em `title:<pso>`. No quadro A, esses intervalos somam aproximadamente 36 ms, dos quais 30 ms em WFI. A execução de referência ficou em torno de 17 FPS. O X2 registrou aproximadamente 670 UAVs do título, 236 UAVs do Victory, 121 globais Enhanced e 2.100–2.500 transições do Victory por quadro.

## Mecanismos e condições de segurança

Os switches aceitam `1` e começam desligados.

### `VICTORY_RF_X7_STATS`

Emite uma linha `rf-x7:` por token de quadro, no mesmo ponto de amostragem do X2. Os contadores vêm das threads de gravação, inclusive thin, de modo que o trabalho gravado adiantado pode aparecer na linha do token anterior. Compare médias de várias linhas para o mesmo trecho de cena.

- `title_uav_named` e `title_uav_global` contam os pedidos, com `Sync` e `BarrierGroups` como um pedido global por chamada traduzida. Os campos `title_uav_emitted_*` contam o que chegou ao host, inclusive esperas geradas ao traduzir barreiras de buffers.
- `victory_global` soma globais Enhanced e UAVs nulos de Victory com instrumentação X2. `victory_transition` conta as transições antes da filtragem. Os mapas atribuem os totais a arquivo, linha, range X2 e, para transições, par de estados em hexadecimal. Cada campo `top_*` mostra até oito entradas. Desconte `narrowed_global` e `dropped_transition` dos respectivos totais para comparar as emissões. `scoped_global` informa quantas globais continuaram globais com escopo Enhanced menor.
- `gaps`, `draws_between`, `dispatches_between` e os máximos descrevem intervalos fechados por duas barreiras traduzidas na mesma gravação de `HostList`. `consecutive` inclui elementos do mesmo array e lacunas observadas sem trabalho entre chamadas. `within_call` e `consecutive_calls` mostram essas parcelas. Cópias, clears e clamps instrumentados interrompem a consecutividade. Execuções indiretas agrupadas contam como um comando host. Listas auxiliares cruas entram nos totais de barreiras instrumentadas e no tamanho de seus arrays.
- `tracked_work`, `possible_uav_work`, `would_omit` e `omitted` mostram a cobertura do rastreador, os comandos com possível uso de UAV, as propostas de supressão e as supressões efetivas.

STATS observa pedidos, emissões e trabalho sem alterar a sequência de comandos. A coleta e as linhas de log adicionam custo na CPU. Os pontos de atribuição usam os mesmos ranges X2 da execução de referência. [Atribuição de WFI no X2](x2-sincronizacao.md)

### `VICTORY_RF_X7_NARROW`

Os clamps simples e em lote declaram os buffers de argumentos, o contador opcional e o buffer de orçamento. O clamp em lote inclui cada fonte instalada na tabela. Quando nenhuma entrada admite UAV, o orçamento é a única dependência UAV do clamp. NARROW usa uma espera por recurso quando a lista tem um buffer de orçamento no cartão, explicitamente transicionado para UAV. O orçamento mapeado usa a espera global legada, inclusive quando ocupa um slot de um slab compartilhado.

Quando o título pode produzir os argumentos ou o contador via UAV, o clamp os lê como SRVs. Nesse caso, com suporte a Enhanced Barriers, a espera sincroniza `ALL_SHADING | CLEAR_UNORDERED_ACCESS_VIEW` antes e `COMPUTE_SHADING` depois, com acessos `UNORDERED_ACCESS → SHADER_RESOURCE | UNORDERED_ACCESS`. O clamp compute lê os argumentos e atualiza o orçamento. A barreira continua global no modo Enhanced para cobrir buffers promovidos no caminho legado sem introduzir estados explícitos para as fontes. A espera original vale se faltar suporte a Enhanced nesse caso, ou se o orçamento estiver indisponível para atribuição. Os endereços, as fontes e os estados dos clamps são os mesmos. A tabela de jobs é preenchida pela CPU. As transições do destino de scratch preservam a ordem entre as leituras indiretas e a escrita do clamp. Os demais recursos da cena continuam a depender das ordens de sincronização explícitas do título e dos auxiliares que os usam. [Semântica de uma barreira UAV legada](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ns-d3d12-d3d12_resource_uav_barrier)

Arrays de transição completos descartam estados de entrada e saída idênticos com flags `NONE`. As transições para COMMON dos scratches simples e em lote no fechamento são emitidas juntas, e o clamp com zero registros reúne as duas transições do scratch em ordem. O preâmbulo grava o aliasing e depois grava as transições que precedem as cópias em uma chamada separada.

As globais em `place.rs` são atribuídas aos chamadores e usam `ALL → ALL` e `COMMON → COMMON`. As máscaras de cópia são compatíveis isoladamente, mas as esperas também interagem com transições legadas do scratch e com shadows cujos acessos e estados anteriores variam dentro de `ExecuteCommandLists`. No caminho legado, a espera inclui aliasing e uma barreira UAV global. [Escopos e interop de Enhanced Barriers](https://microsoft.github.io/DirectX-Specs/d3d/D3D12EnhancedBarriers.html)

### `VICTORY_RF_X7_TITLE_UAV`

O build 523f3ab registrou por quadro 727 pedidos globais de UAV do título e 666 emissões, com zero pedidos nomeados. Quase todos chegaram por `BarrierGroups`. `tracked_work=0` media apenas o subconjunto cujo footprint podia ser enumerado, limitado a raiz dedicada e, para draws, sem entradas de IA nem alvos. O caminho thin chegava aos ganchos de draw e dispatch, mas o filtro excluía os comandos da cena medida.

Agora o ponto compartilhado que registra cada draw, dispatch ou execução indireta host registra o trabalho. A primeira espera global da lista permanece para cobrir trabalho de listas anteriores. Depois, uma nova espera global só é candidata a supressão se o intervalo anterior carecer de acessos UAV possíveis. A classificação reúne `stores`, `stores_by_address`, bindings UAV de raiz e de tabelas, tabelas indexadas e declaração de heap bindless. A presença de um UAV de leitura também mantém a espera porque ela pode ordenar uma leitura anterior antes de uma escrita posterior. Um comando sem metadados seguros e trabalho auxiliar que pode acessar ou copiar dados conservam a espera seguinte. Mudar alvos ou estado de rasterização por si só preserva o estado de dependência.

As esperas mantidas são gravadas no ponto do pedido original. As transições de texturas gêmeas mantêm chamadas separadas e os grupos Enhanced conservam as máscaras originais.

### `VICTORY_RF_X7_VERIFY`

Emite as barreiras originais do título nas posições originais e conta as candidatas do rastreador. A linha `rf-x7:` sai mesmo sem STATS. `would_omit` conta as esperas sem acesso UAV possível desde a última espera global. `omitted` fica em zero. `tracked_work` conta comandos shader registrados, e `possible_uav_work` quantos deles podem acessar UAV. A preservação de conteúdo e estabilidade visual precisa ser conferida no jogo.

### `VICTORY_RF_X7_VALIDATE`

Valida as descrições de todos os grupos Enhanced e dos arrays legados que passam pelo gravador instrumentado, antes de enviá-los ao D3D12. Verifica máscaras de sync e access, tipo de lista, layouts, tipo de recurso e flags de criação. Quando encontra uma descrição inválida, omite o grupo ou array e registra `rf-x7: invalid barrier group skipped` com a lista, o ponto de chamada, o índice e os campos da barreira. A checagem é local a cada chamada. O histórico de estados entre listas, os pares split e as dependências reais dos shaders exigem informação adicional.

## Roteiro antes/depois

Edite `%LOCALAPPDATA%\victory\victory-settings.txt` e reinicie o Victory entre rodadas. Cada atribuição ocupa uma linha no formato `NOME=1`. Remova a linha ou atribua `0` quando a coluna estiver desligada.

| Rodada | `VICTORY_RF_X7_STATS` | `VICTORY_RF_X7_NARROW` | `VICTORY_RF_X7_TITLE_UAV` | `VICTORY_RF_X7_VERIFY` |
| --- | ---: | ---: | ---: | ---: |
| Referência instrumentada | 1 | 0 | 0 | 0 |
| NARROW isolado | 1 | 1 | 0 | 0 |
| Simulação do título | 1 | 0 | 1 | 1 |
| Otimização do título | 1 | 0 | 1 | 0 |

1. Preserve resolução, perfil Series S, cena, percurso, estado de X4, opções de frame generation e versão do driver. Capture depois do aquecimento de cada versão. Use `VICTORY_RF_MARKERS=2` em todas as capturas comparadas.
2. Agregue apenas intervalos-folha em `GPUTRACE_REGIMES`. Compare tempo de GPU por quadro, `fe__cycles_stalled_cmd_wfi_queue_sync`, tempo aproximado em WFI e utilização dos SMs para `title:<pso>` e para os ranges de clamp e preâmbulo. Registre FPS do jogo na mesma janela.
3. Alinhe cada captura com os campos `rf-x7:` e `rf-x2:`. Na simulação, confira a quantidade que seria omitida, a cobertura e a quantidade de comandos com possível uso de UAV. Na rodada ativa, compare os UAVs emitidos, os comandos por lacuna e o WFI, e confira a cena quanto a flicker, texturas faltantes, geometria e argumentos indiretos.
4. Depois da comparação isolada, repita com os dois switches de otimização juntos para medir interação. Uma amostra adicional sem STATS e sem marcadores mede o custo da instrumentação.
5. Para investigar `Close` com `0x80070057`, habilite `VICTORY_RF_X7_VALIDATE=1` junto com NARROW e TITLE_UAV, entre no mundo e procure `invalid barrier group skipped` no log. A omissão de uma barreira inválida pode afetar o trabalho que dependia dela.

A captura BASE fica como referência de leitura. As medições pós-X7 e a execução dos testes unitários ficam para a validação do usuário.
