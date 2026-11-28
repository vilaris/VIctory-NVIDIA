# X5 — Submissão e guardas de escrita

Na cena pesada em Series S, com X4 ligado, a referência recebida é de 12,7–14,4 FPS e 70–79 ms por quadro. A thread de submissão gastava cerca de 48 ms por quadro, com 4,3 ms no watch, 5,2 ms em flights, 30,9 ms nos passos e 8,8 ms em card. Os refreshes ocupavam 17,6 ms dentro dos passos. `front-wall` respondia por cerca de 16,5 ms desses refreshes. A estimativa de 4–7 mil faults de guarda por quadro vinha da contagem de faults do processo. X5 mede diretamente essa parte. Ainda faltam as medidas do build X5.

## Instrumentação

`VICTORY_RF_X5_STATS=1` emite `rf-x5:` a cada cinco segundos de janelas do medidor de FPS. Todos os valores de trabalho são por quadro renderizado. `title` e `runtime` mostram quantidade de faults de escrita atendidos pelo watch e milissegundos acumulados no handler. A classificação usa o endereço da instrução ou da entrada da thread em uma imagem `victory-image*` e memoriza quando uma thread atende um fault em código do título. Threads de entrada intermediada podem ficar em `runtime` até aparecer esse sinal. O timer inclui espera no handler. Esse tempo sobrepõe o trabalho de outras threads e não deve ser somado a `preparing`.

`vp` mostra chamadas a `VirtualProtect` do watch, páginas de 4 KiB pedidas e milissegundos dentro das chamadas, incluindo falhas e restaurações. A proteção de demanda do C2 usa `PAGE_NOACCESS` e tem contabilidade própria. O top 10 agrupa os eventos pelo heap de descritores ou buffer encontrado no endereço inicial do evento, arredondado ao bloco do watch. Uma chamada sobre vários blocos entra uma vez, com todas as páginas pedidas. O ranking usa a soma dos tempos do handler e das chamadas de proteção, que podem se sobrepor. A tabela de endereços tem capacidade fixa para que o handler registre com atômicos, sem alocação ou lock adicional. Os totais incluem endereços sem objeto conhecido, colisões da tabela e objetos já liberados. Os snapshots dos atômicos podem distribuir eventos na fronteira entre duas janelas.

`policy` informa quantos blocos passaram a ficar abertos por cada política e os refreshes e MB lidos pelo caminho direto. `preparing` e os componentes de `submit-ms` usam os mesmos contadores de FPS e de X3. `refresh` está dentro de `steps`.

## Blocos reescritos

`VICTORY_RF_X5_ALWAYS_DIRTY=1` observa faults que devolvem escrita a um bloco de 64 KiB. O histórico tem um bit por quadro e deduplica faults do mesmo bloco no mesmo quadro. Após uma janela completa, pelo menos 90% de quadros com fault qualificam o bloco para ficar aberto. Quadros sem fault entram como zero nessa estimativa. A política atua em views registradas com um endereço de título para o bloco e com o rastreamento de aliases habilitado. Views desconhecidas ou aliased mantêm a estratégia normal. O heap de 174 MB só se beneficia na medida em que seus blocos atendam a essas condições.

Por padrão o bloco fica aberto por 32 quadros. O primeiro consumidor depois disso tenta rearmar a guarda e republicar o resultado. A volta é confirmada com quatro quadros de teste sem fault. Um fault nesse intervalo abre outra pausa. Assim o tempo em que a guarda esteve desligada não conta como evidência de baixa atividade. A taxa e os testes observam faults, então escritas mediadas pelo runtime ou que não mudam a proteção podem limitar a identificação de blocos quentes.

**Coerência.** Um bloco entra na política depois de aberto pelo fluxo normal, sem proteção limpa a remover. Enquanto estiver deliberadamente aberto, não satisfaz `published`, cobertura das consultas adiantadas, nem assinatura do memo. Um refresh inteiramente coberto pelos blocos ainda em pausa toma os bytes sem consultar o kernel ou o journal, sob o mesmo lock de `Shadow::copy_stale`. Uma faixa mista continua perguntando pelos demais blocos e recebe os abertos como novos, no journal. O caminho direto usa `Shadow::take_all` dentro dos trechos livres de `Shadow::update`, preservando a contabilidade de uploads e os trechos pendentes de GPU. Nenhum stamp de limpeza é emitido pela cópia direta. Ao sair da pausa, a primeira consulta que efetivamente rearma publica o write de todo o bloco antes de permitir reuso dos stamps.

**Flights, aliases e remaps.** `Shadow::stale_under_writes` e `merge_under_writes` tratam interseções deliberadamente abertas como motivo para o settle existente. No primeiro acesso que decide adiar a guarda, a mescla exata, se usada, recebe a página marcada por inteiro e compara os bytes como antes. `watch::written_since_taken` e `Host::fill_is_titles` deixam de tomar uma guarda ausente por evidência de escrita do título em blocos abertos pela política. `cover` não considera um alias limpo se depender de um bloco deliberadamente aberto. `views_mapped`, `title_memory_changed` e `title_memory_freed` mantêm a invalidação de views e a troca de endereço descarta o histórico afetado. `Shadow::update` retorna cedo para buffers in-place. A consulta que pode rearmar blocos abertos fixa a faixa com `demand::pin_range` fora do lock de guardas. Isso materializa blocos estacionados no C2 antes de mudar a proteção. A cópia direta usa `copy_mapped`, que também materializa os bytes de demanda.

## Rearme sob demanda

`VICTORY_RF_X5_LAZY_REARM=1` adia a guarda de um bloco único que foi aberto por um fault durante a submissão atual. O próximo consumo em uma nova submissão volta a poder armá-lo. A abertura usa o número da submissão, por isso outros consumidores do mesmo quadro podem rearmar em submissões seguintes. Quando ninguém usa o bloco, nenhuma passagem global tenta rearmá-lo. Registrar um `Tracked` também deixa a proteção para a primeira leitura. Nas consultas que rearmam, os blocos elegíveis consecutivos são reunidos numa chamada por faixa. Buracos, blocos protegidos, aliases e períodos de repouso separam as faixas. O fallback de Windows para uma faixa recusada continua bloco por bloco.

**Coerência.** O intervalo em que a guarda fica aberta tem as mesmas regras de cópia, memo, journal e flights dos blocos quentes. O registro de `Tracked` comprova que a região pode ser rastreada e a primeira leitura faz a consulta de produção. Enquanto nenhum leitor consumiu uma view nova, não há stamp de cópia que possa justificá-la como atual. Uma chamada ao kernel sobre memória privada continua usando write-watch. Nenhum sinal de fence ou envio de lista muda de posição.

## Offload

`VICTORY_RF_X5_OFFLOAD` ficou sem implementação. `Batch::refresh_front` já distribui o trabalho com `shadow::wide_each` e termina todas as cópias antes de `run_deferred`. `Batch::add` usa o mapa `ahead` como prova de que os bytes já existem, e os passos podem gravar dados, terminar a consulta compartilhada ou disparar snapshots. A ordem atual também mede a parede do grupo inteiro em `front-wall`. Provar uma espera mais tardia exigiria registrar as dependências por faixa física, incluindo aliases e escritas CPU que invalidam o watch.

Em `card_before`, o número de envio e a lista de dirty ranges são tomados sob a disciplina de ordem de `QUEUE_ORDER` e `CARD_WORK`. `send::take` tira a cópia privada dessa submissão antes que os bytes de referência possam mudar. A preparação das palavras já usa `land::each_under`, e os trechos grandes usam os mesmos workers via `shadow::copy_all`. `land::Pool::each` é síncrono e o chamador também trabalha. Adiar esses jobs ou o preparo do snapshot até outro ponto exigiria congelar os intervalos de origem e provar sua vida útil até o join, sob as mesmas fences.

## Roteiro no Series S

Grave as chaves como `CHAVE=valor` em `%LOCALAPPDATA%\victory\victory-settings.txt`, pois o launcher usa esse arquivo para montar o ambiente. Reinicie o processo em cada passagem. Use o mesmo build, save, trecho de cena, configuração de X3/X4 e frame generation. Descarte o aquecimento e compare janelas de duração equivalente. Mantenha isto fixo nas comparações instrumentadas.

```ini
VICTORY_RF_X3_STATS=1
VICTORY_RF_X5_STATS=1
VICTORY_RF_X5_DIRTY_RATIO=0.9
VICTORY_RF_X5_DIRTY_WINDOW=32
VICTORY_RF_X5_DIRTY_QUIET=4
VICTORY_RF_X5_DIRTY_PROBE=32
```

| Passagem | `VICTORY_RF_X5_ALWAYS_DIRTY` | `VICTORY_RF_X5_LAZY_REARM` |
| --- | --- | --- |
| Referência | 0 | 0 |
| Blocos quentes | 1 | 0 |
| Rearme sob demanda | 0 | 1 |
| Combinação | 1 | 1 |

Compare FPS e `preparing`, depois `submit-ms.watch`, `flights`, `steps`, `refresh` e `card`. Cruze `front-wall`, `watch-guards`, `update-copied` e MB de refresh do X3 com `title`, `runtime`, `vp` e `policy` do X5. Mais cópias ou settles podem anular o ganho dos faults, particularmente em buffers parcialmente reescritos ou com UAVs. Verifique fidelidade em streaming, transições de cena, render targets e passagens compute que produzam dados para submissões seguintes. Use também `VICTORY_REFRESH_CHECK=1` e os modos de verificação de X3/X4 disponíveis em passagens de correção, com o desempenho comparado em passagens equivalentes. Uma passagem final com estatísticas desligadas mede o ganho sem o custo de instrumentação. Os testes unitários cobrem janela móvel, histerese a partir do rearme e união das faixas. A compilação e a execução ficam com o usuário.
