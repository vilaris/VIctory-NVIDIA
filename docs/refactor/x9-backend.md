# X9 · Coerência de memória e submissão

O branch parte de `399ebd4`. O X9 entrega abertura de guardas por bitmap, refresh por intervalos, um planificador para listas já destacadas, preparação paralela de uploads, descoberta de buffers bindless na submissão e envio de faixas CPU-stream sem comparar bytes. O backend completo continua pendente. A faixa mista de 174 MB conserva compare e merge onde um UAV pode escrever. A descoberta de texturas bindless ainda espera na tradução. O lock de submissão serializa as fases que alteram o estado de coerência. O alvo de 33 ms permanece sem validação.

As referências de desempenho do coordenador são cerca de 45 ms de preparação olhando para uma parede (17,5 ms e 37 MB em cópias) e 93,5 ms dirigindo (50 ms e 85 MB em cópias, mais 19 ms e 259 MB em texturas). Os dois cenários exercitam custos diferentes. Até a fase de staging, as mudanças atuam nos dois eixos, usando os mesmos recursos e comandos da GPU.

No build integrado `0619774` com o X9 inicial, a cena cheia ficou em 16,3–17,6 FPS e aproximadamente 53 ms na thread de submissão. Olhando para a parede, a preparação levou 45–48 ms e o jogo ficou em 18,9–19,7 FPS. O `flights` então publicado pelo X9 somava o merge de bytes com a espera inicial de dependências. A linha X5 e `putting the title's bytes under flights' writes` representam só a segunda etapa de preparação, depois da espera inicial. A nova telemetria distingue `flights` de `flight-prereq`. O resultado agregado de `0619774` não separa quanto do aumento veio de merge ou de espera.

## Fluxo implementado

```text
ExecuteCommandLists do título
  -> queue::behind se houver relay e cortes
       -> planejar índices de listas destacadas numa fila CPU limitada
       -> espera da fila do título resolvida, na ordem da fila
  -> Host::submit
       -> resolver buffers bindless e inserir refreshes em suas posições originais
       -> dependências de flights e consulta ao watch
       -> planejar intervalos e lançar refreshes elegíveis no pool
       -> executar passos CPU ordenados e montar listas
       -> join da última onda de buffers, texturas e fills
       -> snapshots e reserva de staging
       -> join do empacotamento de payload, pares e tiles
       -> join da rodada dos heaps e publicação de slots prontos
       -> ExecuteCommandLists + Signal
  -> relay aguarda fence host
       -> readback, merge, texback e fences observáveis pelo título
       -> aposentar a fatia de upload usada pela submissão
```

A fila mantém a ordenação por fila do título, incluindo cortes, esperas de outras filas e sinais. A captura usa a fila limitada existente. N+1 pode ser preparada enquanto N executa na GPU se as dependências de flights permitirem. O lock de submissão serializa a preparação. O patch reaproveita pools e filas existentes e não registra novas barreiras.

O planificador CPU pode indexar as listas de N+1 antes de N ser enviada. Os índices representam etapas de coerência e declarações de escrita em listas já destacadas. O planificador não lê memória do título e seu resultado vale para um único ciclo de gravação. Se uma lista for reciclada, o resultado antigo perde o vínculo com o novo ciclo. Fila cheia ou plano ainda pendente recebe a varredura inline. A descoberta bindless invalida qualquer plano de índices que ela expandir. O assembler de prelúdios, as esperas e o envio usam o agendador anterior.

O pool recebe uma cópia ou grupo de cópias por job. Cada job processa os intervalos de uma cópia em série. A lista só consome a cobertura após o join. `Batch::title_written` termina o trabalho pendente e limpa a cobertura de buffers e texturas. Uma declaração GPU conserva o envelope do legado. A consulta antecipada ao watch e o teste de interseções com flights usam envelopes conservadores. A transferência pode usar a união das faixas explícitas em buffers host mapeados sem declarações ou flights de escrita e em submissões sem leitor bindless identificado.

```text
Copy / Placed / Run que podem escrever memória do título
  -> terminar as leituras pendentes -> efetuar a escrita -> abrir a onda seguinte

Depois do último passo CPU
  -> [refresh por shadow | detile por staging | fills por destino]
  -> join -> snapshots e ranges sujos
  -> [payload | tabelas de tiles | pares de palavras]
  -> join -> gravar exatamente os comandos de upload existentes

Resolveu flight ou passo CPU alterou heap depois do início da rodada
  -> rodada iniciada após a escrita -> join -> make_waiting -> Execute
```

O caminho de empacotamento reúne três fases CPU numa onda. A gravação dos comandos continua depois do join. A última onda de refresh mistura classes que só leem a memória do título e escrevem destinos separados. `fill_jobs` preserva a ordem dos fills quando os destinos se sobrepõem, inclusive em cadeias de sobreposição. Os steps que escrevem memória do título e as declarações GPU mantêm os respectivos pontos de ordenação. Em uma submissão que agrupa vários cortes, o planejamento antecipado de buffers mantém a cobertura conservadora do legado.

## Autoridade dos bytes

| Classe | Escritor e observação | Entrega dos bytes e transições |
| --- | --- | --- |
| CPU-stream | O título escreve os dados que `Fill` lê por extensão explícita. Para buffers privados com write-watch, o kernel indica as partes escritas de 4 KiB. Um trecho de refresh com observação pelo write-watch e sem um escritor GPU alcançável pode ser copiado diretamente para o staging do shadow, sem `word_runs`. | A informação de escritor é publicada ao criar a UAV tipada e sua versão raw, e ao declarar uma escrita ou ligar a UAV raiz gravável. Essas faixas são cumulativas durante a vida do shadow, pois listas gravadas podem reutilizar views. Um refresh que intercepta essas faixas conserva compare e merge. Os stamps e a ordenação dos envios são os mesmos para ambas as rotas. O scratch de constantes continua ligado à lista. Envios carded usam uma fatia do ring de upload de 32 MiB até `Host::card_done` e recorrem ao pool anterior se preciso. |
| CPU-static | O título ou o runtime pode reescrever os dados. As views de seção usam guardas ou notificações de escrita. | Os bytes novos seguem nos uploads existentes antes dos leitores. Buffers carded conservam staging e VRAM. Um remap invalida as observações. Uma abertura por fault, por si só, não prova que um trecho pode ser promovido a CPU-stream sem guarda. |
| GPU-own, pendente | Uma declaração UAV pode cobrir uma view muito maior que as escritas reais. O buffer de aproximadamente 174 MB mistura constantes do título, geometria e saída de skinning. | A comparação e o merge existentes determinam o que a GPU escreveu. `demand` ainda pode estacionar alguns trechos na VRAM. Remover staging e comparações exige delimitar quais escritas chegam ao intervalo e garantir que acessos da CPU aguardem a versão correta. |
| Descritores | O título escreve por API, por bytes ou por passos CPU da submissão. O espelho consome páginas alteradas do cursor `Tracked`. A comparação agrupa registros por blocos e recoloca leituras incompletas na rodada seguinte. | A publicação passa por `mirror` e `make_waiting`. Samplers com indexação direta dispensam a espera inicial de `note_indexed`. A lista retém a referência guest dos heaps com buffers bindless. Uma rodada após as esperas da fila alimenta a descoberta dos buffers e o planejamento junto com os outros refreshes. O host varre na primeira submissão do quadro e quando novos slots de buffer estão pendentes. `early_textures`, bakes e descoberta de texturas continuam na tradução. |

**DirectStorage e remap.** As notificações do xmem propagam escritas por alias ao watch e invalidam descritores dependentes do mapeamento. Para aberturas por bitmap, um remap fecha a entrada atômica, espera as aberturas em curso e muda as views. A abertura fixa o mapeamento somente com a entrada aberta. Views aliased, materialização e conflitos de proteção usam o tratamento existente.

**Copy-back e texback.** As faixas de shader em voo ainda impedem refreshes que sobrescreveriam seus bytes. O relay mantém as fences, as comparações e o merge. O readback imediato ou sob demanda segue `demand`. O texback de água e física usa o caminho existente, invalidado nos remaps. Para remover a cópia CPU, a prova teria de incluir esse caminho e os acessos CPU fora de `safecopy`. O fetch de demanda atual pode recorrer à fila dedicada preexistente. A substituição precisa ordenar a leitura na fila host sem depender de uma thread parada no fault.

## Handler de escrita

```text
fault em bloco elegível de 64 KiB abaixo de 4 GiB
  -> fixar o mapeamento e bloquear mudança para PAGE_NOACCESS
  -> marcar OPENING / BESIDE, revogar PUBLISHED, avançar a geração
  -> VirtualProtect, limpar PROTECTED e marcar no bitmap
  -> devolver a escrita à thread do título

próximo detentor de GUARDS
  -> drenar o bitmap, reconciliar estados e liberar OPENING
  -> próximo consumo publica o resultado no journal
```

A abertura elegível evita `GUARDS`, a fila `OPENED` e alocação de metadados dentro da exceção. O bitmap é pré-alocado e marca a granularidade em que a proteção é removida. A abertura e o estacionamento usam operações ordenadas atomicamente para impedir que as duas rotas mudem a proteção ao mesmo tempo. O fallback ainda tem esperas e locks. Os faults tratados pelo bitmap entram na estatística própria de X9. O histórico de rearme de X5 observa os faults processados pelo seu caminho.

`VICTORY_RF_X9_FAULTS=1` inclui no histórico de rearme o tempo de cada abertura do bitmap, publicado por bloco junto com a abertura. O handler escreve em armazenamento pré-alocado em O(1). A atualização do histórico roda fora do handler, ao drenar as aberturas, com uma busca por bloco. Um bloco de descritores que acumula custo pode ficar aberto temporariamente quando pertence a uma única view e não é usado como shadow de buffer. Até 1 MiB por quadro pode receber esse tratamento, incluindo releituras dos blocos já abertos. O espelho compara os registros desses blocos a cada consumo, pois podem ser reescritos sem novo fault. A memória privada que já está sob write-watch continua no caminho do kernel.

O indicador `descriptor-rearms-deferred/frame` conta os rearme adiados. O teste `repeated_descriptor_rounds_bound_extra_reads_and_reduce_rearms` modela 24 rodadas de quatro heaps e verifica o limite de releitura, o número de rearme e o fim da pausa. A faixa mista de 174 MB conserva suas proteções nesta mudança. Aliases de seção e remaps continuam no tratamento do watch e de xmem.

## Telemetria e verificação

- `VICTORY_RF_X9_BINDLESS_INCREMENTAL=1` habilita a correção experimental do volume de refreshes do heap. Na primeira submissão do quadro, cada heap mantém a cobertura por cópia em uso e conserva o ciclo periódico de revisão de slots para detectar recursos liberados e views que ganharam uma cópia. Nas demais submissões do quadro, o bitmap resumido visita os grupos marcados e a resolução usa apenas os slots alterados. Os refreshes de delta recebem somente os intervalos descobertos que ainda faltam na cobertura do quadro. A primeira passagem custa O(slots revistos + capacidade do vetor de views + cópias em uso). Uma passagem de delta custa O(capacidade do heap / 4096 + slots marcados + intervalos de delta), mais as operações sobre os intervalos de cada cópia. Não percorre o conjunto de views em uso. O caminho conserva a cadência de atualização por quadro para views que a cobertura já contém. Reescritas de bytes no mesmo intervalo entre submissões exigem validação no jogo, inclusive as que acompanham mudanças de descritor.
- `refreshes/frame` informa flushes, checagens programadas, checagens limpas e checagens paralelas. `bindless-buffers/frame` informa passagens iniciais e posteriores, slots examinados e refreshes inseridos. Compare a build com a regressão de 66.642 checagens por quadro com a referência de cerca de 10.000. O teste `bindless_changes_do_not_reschedule_the_heap_for_each_submission` modela 4 heaps, 48 submissões por heap, 81.000 slots e 2.500 cópias por heap. A expectativa é 10.188 checagens de heap, com um intervalo novo por passagem de delta. O teste contabiliza o teto de bytes programados e quantas passagens produzem refreshes. `dirty_buffer_summary_drains_only_changed_words` exercita a deduplicação de índices e o esvaziamento do bitmap.
- `VICTORY_RF_X9_STATS=1` emite `rf-x9:` na janela do medidor de FPS, em torno de cinco segundos. `submit-ms` é tempo de parede por quadro. `refresh` está dentro de `steps`. `jobs` soma chamadas que podem rodar no pool ou na thread chamadora. `upload-pack`, `texture` e `fill` detalham trabalho das ondas. Captura inclui backpressure. `record-heap-wait` mede chamadas que iniciam ou aguardam a rodada durante a gravação.
- `flights` corresponde ao custo de bytes sob flights na linha X5. `flight-prereq` mede a espera inicial. `merge` mede `stale_of`, incluindo rechecagens, e `settle-and-recheck` inclui eventuais rechecagens internas. Esses valores podem se sobrepor. `merge-fallbacks` conta casos em que a consulta, a referência ou o orçamento de runs exigiram settle. X9 usa um snapshot dos stamps de 4 KiB do journal para as comparações que precisam deles, e percorre só páginas escritas quando a faixa inteira já foi inicializada. A solicitação antecipada de descritores e o refresh adiantado usam novamente os switches anteriores em vez de serem ligados automaticamente pelo X9.
- `planning` mede a construção dos índices CPU e `heap-discovery` mede a resolução dos buffers. O custo da resolução integra o total da linha `preparing per frame, the submitting thread`, inclusive quando é preciso resolver antes de dividir a submissão.
- Os bytes de refresh são agrupados por evidência de write-watch, guarda e memória sem tracking. `card-up` e `gpu-down` medem payload das operações da VRAM. `upload-ring` mede bytes reservados, incluindo o material de scatter e seu alinhamento. `texture-read` conta bytes de leitura na preparação de texturas. Refresh e upload da VRAM podem contabilizar os mesmos bytes em etapas diferentes. `gaps-excluded` mede lacunas excluídas do intervalo, inclusive bytes que o legado poderia reconhecer como limpos. `watch` contabiliza mudanças no mecanismo de observação do shadow. `parked-eager` conta novos blocos preferidos para landing imediato.
- `VICTORY_RF_X9_VERIFY=1` com X9 ativo compara prefixos de até 64 bytes de fills e de intervalos elegíveis com uma leitura pelo endereço do título. Também compara a origem de DMA de um envio direto por shadow com os bytes correspondentes do shadow legado. O verificador dos descritores usa um cursor legado independente e observa a chave publicada. `rf-x9 diverge` informa a faixa e a classe. A comparação após o scatter, dos bytes finais em VRAM, de GPU-own e de texback continua pendente.

## Limites de fidelidade e desempenho

- As declarações UAV de raiz e por indexação podem cobrir mais bytes que os shaders escrevem. Tratar a declaração inteira como propriedade exclusiva da GPU descartaria escritas válidas da CPU. O backend mantém a autoridade compartilhada, os hashes e o merge nessas faixas. Uma migração completa exige registrar a cobertura efetiva dos produtores, tomar posse antes do primeiro leitor da CPU e resolver remaps e aliases de DirectStorage enquanto a proteção é válida.
- A descoberta de buffers em `heap_buffers` vai para a submissão, mas o upload de texturas que o método agenda e as passagens de `early_textures` e de bake continuam na gravação. A migração do lado de texturas exige manter transições do consumidor, validade dos níveis e publicação depois da prontidão do upload, inclusive nos bakes.
- O trabalho paralelo de CPU ocorre dentro da preparação de uma submissão, além do relay já existente. As versões de staging e de escrita vivem parcialmente nos próprios objetos. Preparar duas submissões CPU em paralelo requer versionar esses dados e transferir a dependência de publicação para a etapa ordenada de execução. O ring dá vida útil independente ao payload carded. Staging de texturas e snapshots conservam o modelo atual.
- O ring mantém 32 MiB adicionais de upload mapeado enquanto o processo está ativo. A primeira alocação pode afetar o aquecimento. As ondas mistas competem pelo pool e pela largura de banda da CPU. Os valores da telemetria e a semântica de publicação dos descritores precisam de A/B no jogo.
- `cpu-stream-direct` evita o compare, mas pode enviar mais bytes que o caminho de diferenças quando poucas palavras mudam numa página de 4 KiB. Compare esse campo com `card-up` e com o tempo de GPU. A evidência de propriedade cobre os intervalos sem escritor GPU conhecido na cópia, sem reclassificar os produtores de skinning que alcançam o heap inteiro.

## Arquivos

| Arquivo em `crates/victory-gpu/src` | Alterações |
| --- | --- |
| `rf_x9.rs`, `lib.rs` | Switches, telemetria, checagem de amostras, `Ranges`, `upload_jobs`, `fill_jobs`, `UploadRing`, bitmap e sincronização atômica de remaps. |
| `watch.rs`, `demand.rs` | `open_x9`, reconciliação em `settle` e arbitragem de proteção com o estacionamento. |
| `host.rs`, `shadow.rs` | `Host::submit`, `Batch::copy_spans`, `Batch::flush_wave`, `FrontSpans`, `Shadow::take`, elegibilidade de refresh, verificação e medidas de cópias. |
| `host/send.rs`, `texture.rs` | `ring_memory`, `take_with`, `Own::record`, `give_back` e medição de `Staging::refresh`. |
| `descriptor.rs`, `rf_x4.rs` | `begin_sync_heaps`, `sync_heaps`, `note_indexed`, comparação em blocos e medição das esperas da gravação. |
| `queue.rs`, `queue/behind.rs` | Medição da captura e seleção da fila existente com os seus pré-requisitos. |
| `host/x9_plan.rs`, `host.rs` | Indexação assíncrona em `jobs_of`, expansão de refreshes bindless e sua contabilidade em `prepare_x9_heap_sweeps`. |
| `draw.rs`, `descriptor/host.rs` | Ganchos `rf-x9` em `heap_buffers` e na ligação da UAV raiz. `buffer_uav` informa as faixas graváveis da vista normal e da raw. |

## Validação no Series S

Grave as chaves em `%LOCALAPPDATA%\victory\victory-settings.txt` e reinicie entre rodadas. Use o mesmo save, trecho, configuração de frame generation e switches X3/X4/X5. Descarte o aquecimento.

| Rodada | `VICTORY_RF_X9` | `VICTORY_RF_X9_VERIFY` | `VICTORY_RF_X9_STATS` |
| --- | --- | --- | --- |
| Referência instrumentada | 0 | 0 | 1 |
| Comparação de bytes | 1 | 1 | 1 |
| Medição | 1 | 0 | 1 |

Repita a cena olhando para uma parede e o trecho dirigindo. Compare cadência real, CPU de submissão, esperas de descritores, tempo de fault, uploads, readbacks e `rf-x9 diverge`. Exercite streaming, cortes de cena com remaps, geometria e skinning, bindless, bakes e água. O bitmap aparece com X9 ativo e abertura elegível. O X9 é ativo quando a chave está ausente. Para retornar à referência, grave `VICTORY_RF_X9=0`.

Os testes novos cobrem união, interseção e cobertura de intervalos, retomada do envelope após declaração GPU, primeira inicialização de páginas sob flights, entrega pelo bitmap, exclusão durante remap, empacotamento de uploads, dependências por destino, aposentadoria fora de ordem no ring e reciclagem de listas enquanto há planos pendentes. Os testes existentes incluem `queue::behind::tests`, `heap_marks::tests`, `written_spans_tests` e os de `watch`, `demand` e `upload`. Compilação, execução dos testes e jogo ficam com o usuário. A validação desta entrega foi feita por inspeção e `git diff --check`.
