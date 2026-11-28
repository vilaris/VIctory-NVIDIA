# X5 — Submissão e guardas de escrita

No Series S, o build `3d5ce5d` com `ALWAYS_DIRTY=0` entregou 14–18 FPS, com 426 faults e 51 ms por quadro nas threads do título. A thread de submissão registrou 4,2 ms em watch, 4,6 ms em flights, 29,7 ms em steps, 6,9 ms em card e 15,1 ms de refresh dentro de steps. O heap de descritores em `0x3340000+0x2600000` respondeu por cerca de 105 faults e 25 ms por quadro. O buffer de cerca de 174 MB também apareceu entre os maiores custos.

No teste isolado com `ALWAYS_DIRTY=1` e `RT_FALLBACK=0`, houve 465 faults e 54 ms por quadro, com 0,7 novos blocos em pausa por quadro. As cópias subiram de 35–47 MB para 64–75 MB por quadro, a espera pela GPU passou de 0–1 ms a 22–60 ms e `flights` chegou a 50 ms. O FPS caiu para 9–11,6. A contagem de `hot-blocks` mede entradas em pausa. Vários blocos podiam permanecer abertos e ser lidos muitas vezes sem voltar a aparecer nessa contagem. A atribuição desse aumento ao buffer de 174 MB permanece sem confirmação por endereço.

## Instrumentação

`VICTORY_RF_X5_STATS=1` emite `rf-x5:` a cada cinco segundos de janelas do medidor de FPS. Todos os valores de trabalho são por quadro renderizado. `title` e `runtime` mostram quantidade de faults de escrita atendidos pelo watch e milissegundos acumulados no handler. A classificação usa o endereço da instrução ou da entrada da thread em uma imagem `victory-image*` e memoriza quando uma thread atende um fault em código do título. Threads de entrada intermediada podem ficar em `runtime` até aparecer esse sinal. O timer inclui espera no handler. Esse tempo sobrepõe o trabalho de outras threads e não deve ser somado a `preparing`.

`vp` mostra chamadas a `VirtualProtect` do watch, páginas de 4 KiB pedidas e milissegundos dentro das chamadas, incluindo falhas e restaurações. A proteção de demanda do C2 usa `PAGE_NOACCESS` e tem contabilidade própria. O top 10 agrupa os eventos pelo heap de descritores ou buffer encontrado no endereço inicial do evento, arredondado ao bloco do watch. Uma chamada sobre vários blocos entra uma vez, com todas as páginas pedidas. O ranking usa a soma dos tempos do handler e das chamadas de proteção, que podem se sobrepor. A tabela de endereços tem capacidade fixa para que o handler registre com atômicos, sem alocação ou lock adicional. Os totais incluem endereços sem objeto conhecido, colisões da tabela e objetos já liberados. Os snapshots dos atômicos podem distribuir eventos na fronteira entre duas janelas.

`veh` separa o tempo esperando `GUARDS` (`wait`), conciliando aberturas pendentes sob o lock (`settle`), abrindo e atualizando as notas locais sob o lock (`locked-work`) e em `VirtualProtect` dentro do handler (`vp`). `vp` pode estar dentro de `locked-work`. `lock-attempts` conta tentativas, enquanto `unlocked` conta aberturas bem-sucedidas sem o lock global. O tempo do handler é tempo decorrido acumulado nas threads que falharam, incluindo espera bloqueada.

`policy` informa quantos blocos passaram a ficar abertos por cada política, quantos rearms X4 adiou (`descriptor-deferrals`) e quantos refreshes e MB o caminho direto de `LAZY_REARM` leu. `preparing` e os componentes de `submit-ms` usam os mesmos contadores de FPS e de X3. `refresh` está dentro de `steps`.

## Blocos caros para rearmar

O critério antigo exigia um fault em pelo menos 29 dos 32 quadros por endereço de 64 KiB e reduzia todos os faults de um mesmo quadro a um bit. Dez reaberturas caras em um único quadro valiam tanto quanto uma, e faults espalhados pelas centenas de blocos de um heap raramente atingiam os 29 quadros no mesmo endereço. A taxa também dependia de quando um consumidor rearmava a página, pois escritas feitas com a guarda aberta não causavam uma exceção.

`VICTORY_RF_X5_ALWAYS_DIRTY=1` usa o custo de todas as aberturas por fault de cada bloco, inclusive várias no mesmo quadro. Por padrão, duas ou mais aberturas com pelo menos 192 µs acumulados em uma janela de 16 quadros qualificam o bloco. O limiar usa o equivalente a 12 µs de handler por quadro com o denominador fixo de 16 quadros. A qualificação vale por até 16 quadros. Depois, o bloco precisa acumular novos faults. `VICTORY_RF_X5_DIRTY_RATIO` e `VICTORY_RF_X5_DIRTY_QUIET` foram aposentadas.

A política antiga retirava o selo de limpo dos blocos em pausa. A partir daí, refreshes de shadow inteiramente cobertos tomavam toda a faixa pedida pelo caminho direto e refreshes mistos recebiam um novo write de 64 KiB por bloco aberto. Cada tomada podia alimentar uploads e uma interseção com trabalho de GPU pendente levava ao settle de flights. Repetir isso a cada consumo explica como poucos blocos novos em pausa podiam fazer as cópias e a espera subirem juntas.

A nova pausa só pode ser aplicada pela leitura de descritores do X4 com `VICTORY_RF_X4_PICKED=1`. O bloco precisa já estar aberto, caber inteiro nos registros lidos pelo X4, ter uma única view registrada e estar fora dos buffers e shadows registrados. O X4 já lê esse bloco na rodada que adiaria seu rearme. Nas rodadas seguintes, o X4 pode ter de reler o bloco e comparar com a imagem anterior. Só registros alterados seguem para a atualização de descritores. Views mapeadas por seção não oferecem a esse watch um bitmap de escrita de 4 KiB quando abertas. O orçamento global limita os rearms adiados a 16 blocos de 64 KiB por quadro (1 MiB de blocos), cobrando também adiamentos repetidos. Após a pausa, a primeira leitura volta a tentar rearmar e publica a escrita pelo journal. Nenhum buffer grande fica deliberadamente aberto por essa política, nem ela aciona a cópia direta de `Shadow`.

## Custo da abertura da guarda

Nos perfis anteriores de `watch.rs`, 252 dos 281 ms acumulados em 2.729 faults foram espera por `GUARDS`, com mediana de 8 µs para o trabalho do fault. No Series S os 54 ms em 465 faults equivalem a cerca de 116 µs por fault. A nova telemetria mede a decomposição nessa máquina. O handler invalida os bits de publicação e devolve a escrita com `VirtualProtect`. A próxima consulta ao watch faz o trabalho de journal. O caminho com lock antes alocava um `Vec` até para abrir um bloco único sem alias. Esse caso agora usa um array unitário na pilha.

`VICTORY_RF_X5_FAST_FAULT=1` usa a abertura existente com `OPENING` para um bloco sem alias, se conseguir fixar o mapeamento sem esperar. Ela chama `VirtualProtect` fora de `GUARDS`. O próximo detentor de `GUARDS` concilia a abertura. Os dois vetores da fila de conciliação são reaproveitados e recebem capacidade inicial fora do handler. Se outra thread já estiver abrindo o mesmo bloco, a espera observa os bits e verifica a proteção uma vez antes de seguir. Aliases e mapeamentos em mudança usam o caminho com lock. O fast path é ligado por padrão quando `ALWAYS_DIRTY=1` e pode ser comparado isoladamente.

## Rearme sob demanda

`VICTORY_RF_X5_LAZY_REARM=1` adia a guarda de um bloco único que foi aberto por um fault durante a submissão atual. O próximo consumo em uma nova submissão volta a poder armá-lo. A abertura usa o número da submissão, por isso outros consumidores do mesmo quadro podem rearmar em submissões seguintes. Quando ninguém usa o bloco, nenhuma passagem global tenta rearmá-lo. Registrar um `Tracked` também deixa a proteção para a primeira leitura. Nas consultas que rearmam, os blocos elegíveis consecutivos são reunidos numa chamada por faixa. Buracos, blocos protegidos, aliases e períodos de repouso separam as faixas. O fallback de Windows para uma faixa recusada continua bloco por bloco.

**Coerência.** A política independente `LAZY_REARM` mantém o caminho direto para refreshes inteiramente cobertos por blocos que ela própria ainda adia. Enquanto uma guarda foi deliberadamente deixada aberta, a faixa fica fora do memo e das respostas adiantadas e uma interseção com ranges pendentes preserva o settle existente. Uma consulta que rearma materializa e fixa a faixa do C2 antes de alterar a proteção. Aliases novos invalidam o adiamento e remaps descartam o histórico afetado.

## Offload

`VICTORY_RF_X5_OFFLOAD` ficou sem implementação. `Batch::refresh_front` já distribui o trabalho com `shadow::wide_each` e termina todas as cópias antes de `run_deferred`. `Batch::add` usa o mapa `ahead` como prova de que os bytes já existem, e os passos podem gravar dados, terminar a consulta compartilhada ou disparar snapshots. A ordem atual também mede a parede do grupo inteiro em `front-wall`. Provar uma espera mais tardia exigiria registrar as dependências por faixa física, incluindo aliases e escritas CPU que invalidam o watch.

Em `card_before`, o número de envio e a lista de dirty ranges são tomados sob a disciplina de ordem de `QUEUE_ORDER` e `CARD_WORK`. `send::take` tira a cópia privada dessa submissão antes que os bytes de referência possam mudar. A preparação das palavras já usa `land::each_under`, e os trechos grandes usam os mesmos workers via `shadow::copy_all`. `land::Pool::each` é síncrono e o chamador também trabalha. Adiar esses jobs ou o preparo do snapshot até outro ponto exigiria congelar os intervalos de origem e provar sua vida útil até o join, sob as mesmas fences.

## Roteiro no Series S

Grave as chaves como `CHAVE=valor` em `%LOCALAPPDATA%\victory\victory-settings.txt`, pois o launcher usa esse arquivo para montar o ambiente. Reinicie o processo em cada passagem. Use o mesmo build, save, trecho de cena, configuração de X3/X4 e frame generation. Descarte o aquecimento e compare janelas de duração equivalente. Mantenha isto fixo nas comparações instrumentadas.

```ini
VICTORY_RF_X3_STATS=1
VICTORY_RF_X5_STATS=1
VICTORY_RF_X4_PICKED=1
VICTORY_RF_X4_STATS=1
VICTORY_FAULT_UNLOCKED=0
VICTORY_RF_X5_FAULT_US_PER_FRAME=12
VICTORY_RF_X5_DIRTY_WINDOW=16
VICTORY_RF_X5_DIRTY_PROBE=16
```

| Passagem | `VICTORY_RF_X5_ALWAYS_DIRTY` | `VICTORY_RF_X5_FAST_FAULT` | `VICTORY_RF_X5_LAZY_REARM` |
| --- | --- | --- | --- |
| Referência | 0 | 0 | 0 |
| Abertura sem lock global | 0 | 1 | 0 |
| Pausa só no X4 | 1 | 0 | 0 |
| Pausa no X4 e abertura sem lock global | 1 | 1 | 0 |

Compare FPS, `title`, `veh`, `submit-ms.flights`, `submit-ms.refresh`, MB de refresh do X3 e `MB-read` do X4. `direct-MB` deve ficar em zero nessas passagens. Verifique fidelidade em streaming, transições de cena, render targets e passagens compute que produzam dados para submissões seguintes. Use `VICTORY_REFRESH_CHECK=1` e os modos de verificação de X3/X4 em passagens de correção. Uma passagem final com estatísticas desligadas mede o ganho sem o custo de instrumentação. Os testes unitários cobrem a qualificação por custo, expiração, orçamento e união de faixas. A compilação e a execução ficam com o usuário.
