# X7 — Barreiras GPU no fluxo do título

## Referência da captura

A referência é `C:\Users\mathe\orca\nsight-captures\gputrace-wfi\BASE`. Na RTX 4070, em Series S com X4 desligado, os dois quadros exportados duraram 62,89 e 60,53 ms na GPU. O FE esteve em Wait-For-Idle por 45–47% do tempo, com utilização dos SMs de cerca de 9%. Cerca de 81% do WFI atribuído aos intervalos-folha está em `title:<pso>`. No quadro A, esses intervalos somam aproximadamente 36 ms, dos quais 30 ms em WFI. A execução de referência ficou em torno de 17 FPS. O X2 registrou aproximadamente 670 UAVs do título, 236 UAVs do Victory, 121 globais Enhanced e 2.100–2.500 transições do Victory por quadro.

## Mecanismos e condições de segurança

Os quatro switches aceitam `1` e começam desligados.

### `VICTORY_RF_X7_STATS`

Emite uma linha `rf-x7:` por token de quadro, no mesmo ponto de amostragem do X2. Os contadores vêm das threads de gravação, inclusive thin, de modo que o trabalho gravado adiantado pode aparecer na linha do token anterior. Compare médias de várias linhas para o mesmo trecho de cena.

- `title_uav_named` e `title_uav_global` contam os pedidos, com `Sync` e `BarrierGroups` como um pedido global por chamada traduzida. Os campos `title_uav_emitted_*` contam o que chegou ao host, inclusive esperas geradas ao traduzir barreiras de buffers.
- `victory_global` soma globais Enhanced e UAVs nulos de Victory com instrumentação X2. `victory_transition` conta as transições antes da filtragem. Os mapas atribuem os totais a arquivo, linha, range X2 e, para transições, par de estados em hexadecimal. Cada campo `top_*` mostra até oito entradas. Desconte `narrowed_global` e `dropped_transition` dos respectivos totais para comparar as emissões. `scoped_global` informa quantas globais continuaram globais com escopo Enhanced menor.
- `gaps`, `draws_between`, `dispatches_between` e os máximos descrevem intervalos fechados por duas barreiras traduzidas na mesma gravação de `HostList`. `consecutive` inclui elementos do mesmo array e lacunas observadas sem trabalho entre chamadas. Cópias, clears e clamps instrumentados interrompem a consecutividade. Execuções indiretas agrupadas contam como um comando host. Listas auxiliares cruas entram nos totais de barreiras instrumentadas e no tamanho de seus arrays.
- `tracked_work`, `delayed_past_work`, `would_omit` e `omitted` mostram a cobertura do preditor, o trabalho atravessado, as propostas de supressão e as supressões efetivas.

STATS observa pedidos, emissões e trabalho sem alterar a sequência de comandos. A coleta e as linhas de log adicionam custo na CPU. Os pontos de atribuição usam os mesmos ranges X2 da execução de referência. [Atribuição de WFI no X2](x2-sincronizacao.md)

### `VICTORY_RF_X7_NARROW`

Os clamps simples e em lote declaram os buffers de argumentos, o contador opcional e o buffer de orçamento. O clamp em lote inclui cada fonte instalada na tabela. O orçamento aponta para a alocação mapeada, o slab que contém o slot ou o buffer no cartão. Quando nenhuma entrada admite UAV, o orçamento é a única dependência UAV do clamp. NARROW grava a espera UAV nesse recurso, entre seus acessos UAV.

Quando shaders podem produzir os argumentos ou o contador, o clamp os lê como SRVs. Nesse caso, com suporte a Enhanced Barriers, a espera usa `ALL_SHADING → COMPUTE_SHADING` e `UNORDERED_ACCESS → SHADER_RESOURCE | UNORDERED_ACCESS`. O produtor pode ser gráfico ou compute, e o consumidor é o clamp compute, que lê argumentos e atualiza o orçamento. A barreira continua global no modo Enhanced para cobrir buffers promovidos no caminho legado sem introduzir estados explícitos para as fontes. A espera original vale se faltar suporte a Enhanced nesse caso, ou se o orçamento estiver indisponível para atribuição. Os endereços, as fontes e os estados dos clamps são os mesmos. A tabela de jobs é preenchida pela CPU. As transições do destino de scratch preservam a ordem entre as leituras indiretas e a escrita do clamp. Os demais recursos da cena continuam a depender das ordens de sincronização explícitas do título e dos auxiliares que os usam. [Semântica de uma barreira UAV legada](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ns-d3d12-d3d12_resource_uav_barrier)

Arrays de transição completos descartam estados de entrada e saída idênticos. As transições para COMMON dos scratches simples e em lote no fechamento são emitidas juntas, e o clamp com zero registros reúne as duas transições na mesma chamada. O preâmbulo reúne seu aliasing e as transições consecutivas que precedem as cópias. A ordem dos elementos permanece igual. A filtragem só usa igualdade de estados com flags `NONE`, preservando layouts, flags e ordem de transições split e de aliasing.

A barreira Enhanced instrumentada de cópia posicionada mantém `ALL → ALL`. O helper atende produtores e consumidores de shader e cópia, aceita buffers promovidos implicitamente pelas barreiras legadas e contempla aliases. O compare de coerência e seus auxiliares mantêm a espera global. Esses pontos precisam de estados e escopos comprovados para os recursos em ambos os lados antes de uma conversão. [Barreiras de recurso D3D12](https://learn.microsoft.com/en-us/windows/win32/direct3d12/using-resource-barriers-to-synchronize-resource-states-in-direct3d-12) · [Escopos e interop de Enhanced Barriers](https://microsoft.github.io/DirectX-Specs/d3d/D3D12EnhancedBarriers.html)

### `VICTORY_RF_X7_TITLE_UAV`

O rastreador é por lista host e começa com trabalho anterior desconhecido. A primeira espera global traduzida cobre esse trabalho. Depois, uma espera global pode ficar pendente em uma região de acessos conhecidos. O preditor compara o conjunto antes da espera com todo trabalho que ela atravessaria. Em RAW, WAR ou WAW a espera sai antes do comando dependente. A interseção usa a identidade do buffer host e a faixa inteira do shadow em memória do título para considerar aliases de cópias distintas. Uma espera global posterior absorve a pendente e cobre o trabalho acumulado desde a última emissão. A lista também materializa a pendência antes de operações opacas e antes do trabalho de fechamento.

As associações conhecidas vêm dos slots dedicados que `root_uav` e `root_srv_table` montam e mantêm durante a gravação. SRVs contam como leituras, e UAVs contam como leitura e possível escrita de acordo com `stores`. A versão inicial cobre dispatches diretos com esses bindings e draws diretos sem entradas do IA, RT ou profundidade, com os mesmos bindings. Os acessos por endereço, picks, slots do heap do título sujeitos a reescrita, CBVs, desenhamento indireto, auxiliares, cópias e outros acessos sem prova encerram a região. A instrumentação X4 de descritores também torna essas associações opacas ao X7. Esses caminhos são tratados como leitura e escrita em qualquer recurso. A análise tem capacidade limitada por região e conserva a espera ao perder precisão.

UAVs com recurso podem absorver uma nova espera do mesmo recurso enquanto nenhum comando conhecido tocar aquele recurso e nenhuma operação desconhecida intervier. A primeira espera específica é emitida. A lista segura a referência ao recurso para a identidade continuar válida. Arrays com aliasing, transições ou flags especiais mantêm sua emissão. A lógica opera nos comandos host compartilhados pela gravação imediata e pela tradução thin.

A cobertura depende dos bindings reais nessa cena. Em particular, uma sequência dominada por descritores mutáveis ou indiretos pode oferecer pouco espaço para redução. Quando uma espera global é movida, o driver ainda pode drenar trabalho independente iniciado antes da nova posição. Compare a taxa de omissão com o WFI medido.

### `VICTORY_RF_X7_VERIFY`

Emite as barreiras originais do título nas posições originais e simula a fusão em paralelo. A linha `rf-x7:` sai mesmo sem STATS. `would_omit` registra fusões e esperas já cobertas, enquanto `omitted` fica em zero. `conservative_hazards` conta possíveis conflitos atravessados no modelo alternativo, que admite escrita por todo UAV e aproxima faixas contíguas como possíveis aliases. `conservative_omissions` conta esses testemunhos nas fusões propostas. Um contador positivo identifica casos para inspeção antes da rodada ativa. O verificador compartilha a observação das associações com o preditor. A preservação de conteúdo e estabilidade visual precisa ser conferida no jogo.

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
3. Alinhe cada captura com os campos `rf-x7:` e `rf-x2:`. Na simulação, confira a quantidade que seria omitida, a cobertura e os testemunhos conservadores. Na rodada ativa, compare os UAVs emitidos, os comandos por lacuna e o WFI, e confira a cena quanto a flicker, texturas faltantes, geometria e argumentos indiretos.
4. Depois da comparação isolada, repita com os dois switches de otimização juntos para medir interação. Uma amostra adicional sem STATS e sem marcadores mede o custo da instrumentação.

A captura BASE fica como referência de leitura. As medições pós-X7 e a execução dos testes unitários ficam para a validação do usuário.
