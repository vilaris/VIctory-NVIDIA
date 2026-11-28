# X7 — Barreiras GPU no fluxo do título

## Referência da captura

A referência é `C:\Users\mathe\orca\nsight-captures\gputrace-wfi\BASE`. Na RTX 4070, em Series S com X4 desligado, os dois quadros exportados duraram 62,89 e 60,53 ms na GPU. O FE esteve em Wait-For-Idle por 45–47% do tempo, com utilização dos SMs de cerca de 9%. Cerca de 81% do WFI atribuído aos intervalos-folha está em `title:<pso>`. No quadro A, esses intervalos somam aproximadamente 36 ms, dos quais 30 ms em WFI. A execução de referência ficou em torno de 17 FPS. O X2 registrou aproximadamente 670 UAVs do título, 236 UAVs do Victory, 121 globais Enhanced e 2.100–2.500 transições do Victory por quadro.

## Mecanismos e condições de segurança

Os quatro switches aceitam `1` e começam desligados.

### `VICTORY_RF_X7_STATS`

Emite uma linha `rf-x7:` por token de quadro, no mesmo ponto de amostragem do X2. Os contadores vêm das threads de gravação, inclusive thin, de modo que o trabalho gravado adiantado pode aparecer na linha do token anterior. Compare médias de várias linhas para o mesmo trecho de cena.

- `title_uav_named` e `title_uav_global` contam os pedidos, com `Sync` e `BarrierGroups` como um pedido global por chamada traduzida. Os campos `title_uav_emitted_*` contam o que chegou ao host, inclusive esperas geradas ao traduzir barreiras de buffers.
- `victory_global` soma globais Enhanced e UAVs nulos de Victory com instrumentação X2. `victory_transition` conta as transições antes da filtragem. Os mapas atribuem os totais a arquivo, linha, range X2 e, para transições, par de estados em hexadecimal. Cada campo `top_*` mostra até oito entradas. Desconte `narrowed_global` e `dropped_transition` dos respectivos totais para comparar as emissões. `scoped_global` informa quantas globais continuaram globais com escopo Enhanced menor.
- `gaps`, `draws_between`, `dispatches_between` e os máximos descrevem intervalos fechados por duas barreiras traduzidas na mesma gravação de `HostList`. `consecutive` inclui elementos do mesmo array e lacunas observadas sem trabalho entre chamadas. `within_call` e `consecutive_calls` mostram essas parcelas. `coalesced_calls` registra chamadas fundidas, e `coalesced_enhanced` registra entradas globais fundidas dentro de grupos Enhanced. Cópias, clears e clamps instrumentados interrompem a consecutividade. Execuções indiretas agrupadas contam como um comando host. Listas auxiliares cruas entram nos totais de barreiras instrumentadas e no tamanho de seus arrays.
- `tracked_work`, `possible_uav_work`, `would_omit` e `omitted` mostram a cobertura do rastreador, os comandos com possível uso de UAV, as propostas de supressão e as supressões efetivas.

STATS observa pedidos, emissões e trabalho sem alterar a sequência de comandos. A coleta e as linhas de log adicionam custo na CPU. Os pontos de atribuição usam os mesmos ranges X2 da execução de referência. [Atribuição de WFI no X2](x2-sincronizacao.md)

### `VICTORY_RF_X7_NARROW`

Os clamps simples e em lote declaram os buffers de argumentos, o contador opcional e o buffer de orçamento. O clamp em lote inclui cada fonte instalada na tabela. O orçamento aponta para a alocação mapeada, o slab que contém o slot ou o buffer no cartão. Quando nenhuma entrada admite UAV, o orçamento é a única dependência UAV do clamp. NARROW grava a espera UAV nesse recurso, entre seus acessos UAV.

Quando shaders podem produzir os argumentos ou o contador, o clamp os lê como SRVs. Nesse caso, com suporte a Enhanced Barriers, a espera usa `ALL_SHADING → COMPUTE_SHADING` e `UNORDERED_ACCESS → SHADER_RESOURCE | UNORDERED_ACCESS`. O produtor pode ser gráfico ou compute, e o consumidor é o clamp compute, que lê argumentos e atualiza o orçamento. A barreira continua global no modo Enhanced para cobrir buffers promovidos no caminho legado sem introduzir estados explícitos para as fontes. A espera original vale se faltar suporte a Enhanced nesse caso, ou se o orçamento estiver indisponível para atribuição. Os endereços, as fontes e os estados dos clamps são os mesmos. A tabela de jobs é preenchida pela CPU. As transições do destino de scratch preservam a ordem entre as leituras indiretas e a escrita do clamp. Os demais recursos da cena continuam a depender das ordens de sincronização explícitas do título e dos auxiliares que os usam. [Semântica de uma barreira UAV legada](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ns-d3d12-d3d12_resource_uav_barrier)

Arrays de transição completos descartam estados de entrada e saída idênticos. As transições para COMMON dos scratches simples e em lote no fechamento são emitidas juntas, e o clamp com zero registros reúne as duas transições na mesma chamada. O preâmbulo reúne seu aliasing e as transições consecutivas que precedem as cópias. A ordem dos elementos permanece igual. A filtragem só usa igualdade de estados com flags `NONE`, preservando layouts, flags e ordem de transições split e de aliasing.

As globais em `place.rs` são atribuídas aos chamadores e usam o lado conhecido da cópia com NARROW. Antes da cópia, `ALL → COPY` ainda cobre todos os produtores possíveis. Entre duas cópias do mesmo buffer, `COPY → COPY` cobre a leitura e a escrita via scratch. Depois, `COPY → ALL` ordena as leituras e escritas da cópia antes de qualquer consumidor. O caminho legado preserva a espera global e o aliasing. A substituição por barreira Enhanced de buffer exige que a origem e o destino estejam em COMMON na fronteira entre APIs. Os shadows já podem ter sido promovidos implicitamente no mesmo ExecuteCommandLists, sem estado legado conhecido por esse helper. Por isso essas barreiras continuam globais com escopo menor. O compare de coerência e os demais auxiliares preservam sua espera global ao dependerem de acessos legados ou aliases. [Escopos e interop de Enhanced Barriers](https://microsoft.github.io/DirectX-Specs/d3d/D3D12EnhancedBarriers.html)

### `VICTORY_RF_X7_TITLE_UAV`

O build 523f3ab registrou por quadro 727 pedidos globais de UAV do título e 666 emissões, com zero pedidos nomeados. Quase todos chegaram por `BarrierGroups`. `tracked_work=0` media apenas o subconjunto cujo footprint podia ser enumerado, limitado a raiz dedicada e, para draws, sem entradas de IA nem alvos. O caminho thin chegava aos ganchos de draw e dispatch, mas o filtro excluía os comandos da cena medida.

Agora o ponto compartilhado que registra cada draw, dispatch ou execução indireta host registra o trabalho. A primeira espera global da lista permanece para cobrir trabalho de listas anteriores. Depois, uma nova espera global só é candidata a supressão se o intervalo anterior carecer de acessos UAV possíveis. A classificação reúne `stores`, `stores_by_address`, bindings UAV de raiz e de tabelas, tabelas indexadas e declaração de heap bindless. A presença de um UAV de leitura também mantém a espera porque ela pode ordenar uma leitura anterior antes de uma escrita posterior. Um comando sem metadados seguros e trabalho auxiliar que pode acessar ou copiar dados conservam a espera seguinte. Mudar alvos ou estado de rasterização por si só preserva o estado de dependência.

Com NARROW, a espera global legada pode ficar pendente durante comandos de estado sem trabalho. A próxima espera global equivalente, ou uma chamada adjacente com transições compatíveis em um ponto explícito de gravação, compartilha o mesmo `ResourceBarrier`. Antes de um comando capaz de gravar trabalho, ou ao fechar a lista, a espera pendente é emitida. Arrays de transições de texturas gêmeas são agrupados com a ordem original. Grupos Enhanced compostos só de globais compatíveis podem unir as máscaras de sync e access. Aliasing, layouts, transições split ou `NO_ACCESS` conservam suas restrições de ordenação. A coalescência entre chamadas usa os pontos em que a gravação comprova adjacência.

### `VICTORY_RF_X7_VERIFY`

Emite as barreiras originais do título nas posições originais e conta as candidatas do rastreador. A linha `rf-x7:` sai mesmo sem STATS. `would_omit` conta as esperas sem acesso UAV possível desde a última espera global. `omitted` fica em zero. `tracked_work` conta comandos shader registrados, e `possible_uav_work` quantos deles podem acessar UAV. A preservação de conteúdo e estabilidade visual precisa ser conferida no jogo.

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

A captura BASE fica como referência de leitura. As medições pós-X7 e a execução dos testes unitários ficam para a validação do usuário.
