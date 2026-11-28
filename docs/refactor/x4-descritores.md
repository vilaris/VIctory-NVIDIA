# X4 · Sincronização do heap de descritores

A captura X3 em modo Series S atribuiu 22,2 ms e 24 MB por quadro ao
`descriptor-sync` do heap `0x240f0000+0x2600000`. O preparo das submissões
levou cerca de 68–73 ms. A GPU levou cerca de 38 ms nos comandos do jogo.
X4 atua nas consultas ao write-watch, nas leituras de registros e no
trabalho de espelhamento da CPU.

## Switches

Os quatro switches são lidos no primeiro uso. Todos começam desligados.
Com todos desligados, o fluxo legado continua com a mesma cadência
e as mesmas decisões de leitura e publicação. As verificações dos
switches ficam no caminho, sem análise de shaders, alocações de buffers
de verificação ou novos relógios por página.

### `VICTORY_RF_X4_STATS=1`

`rf-x4:` agrega o trabalho por quadro, em janelas de cerca de cinco
segundos. `pages-requested` conta páginas de 64 KiB cobertas pelos intervalos
pedidos a `Tracked`. `pages-dirty` deduplica as partes de uma mesma página
na mesma rodada. `ring` conta as páginas em que mais de 90% dos registros
alterados foram deixados para tabelas. `other` é o restante.

`slots read` mede os registros apresentados ao comparador. `compared`
conta as comparações individuais após o teste em bloco. `mirrored` conta
os pedidos de espelhamento, incluindo os registros que saíram da espera.
`MB-read` está em MiB. `asking-ms` e `making-ms` medem tempo de parede
da rodada. `reading-ms` e `comparing-ms` somam tempos dos workers.
Na rota `VICTORY_SYNC_IN_PLACE=1`, o tempo da operação combinada entra
em `reading-ms`.

`possible-picked` conta alterações dentro de um intervalo que um shader
gravado pode alcançar, mantido enquanto o heap vive para incluir listas
pendentes ou reenviadas. É um limite superior por quadro. `table-only`
conta as alterações fora desse intervalo com marca recente de tabela.
`unknown` cobre as demais. Os três campos contam eventos de alteração,
portanto uma reescrita do mesmo slot pode entrar mais de uma vez. Os
índices executados de fato pela GPU exigiriam feedback de shader. A
telemetria sozinha preserva o sinal legado de submissão e as publicações.

### `VICTORY_RF_X4_PICK_GATE=1`

Na fila late com cortes em esperas, o gate desliga rodadas especulativas
por temporizador e pedidos antecipados que desconhecem as listas que
efetivamente vão rodar. A submissão usa o OR dos bits
`HostList::heap_picked` já consumidos por `Host::submit`. O bit segue a
lista gravada entre threads, cortes, esperas e reenvios. O reset da lista
limpa o bit. Uma submissão com qualquer lista marcada retém a rodada e
a espera do fluxo late, inclusive quando é preciso repetir a rodada
depois de esperar escritas de voos anteriores.

`heap_buffers`, `heap_strides` e `lent_heap` já marcavam a lista.
X4 também cobre imagens e samplers por índice sem SRV de strides. A
gravação consulta os bits de indexação direta do `SFI0` emitido pelo
tradutor e a procedência dos descritores no rastreador de recursos.
Uma tabela sem binding válido recebe o tratamento conservador. A
configuração de filas síncronas conserva as rodadas especulativas.

Uma rodada omitida deixa o cursor `Tracked::seen` quieto. Mesmo quando
outro consumidor consulta o write-watch, o journal preserva as escritas
para o cursor de descritores até a próxima rodada necessária. Espelhamento
por chamada de API e preenchimento de tabelas durante a gravação continuam
nas entradas existentes.

### `VICTORY_RF_X4_PICKED=1`

O intervalo por heap é vazio até aparecer um leitor que possa indexar
slots do título. Para provar que um acesso é de tabela, o rastreador
precisa encontrar um deslocamento estático alinhado e uma entrada
correspondente em `Translated::used`, consumida por `State::tables`. As
tabelas diretas têm o slot preparado pela própria gravação. As tabelas
que exigem snapshot usam os slots dinâmicos do host. A prova é cacheada
por shader em cada thread de gravação.

Na fila late com cortes, heaps que só receberam os acessos de tabela
provados podem ficar fora de toda a rodada. Seu cursor não avança.
O primeiro leitor por índice publica a marca do heap e espera uma
rodada nova, incluindo as partes sujas acumuladas, antes de usar os
passes de buffers e texturas. A marca fica no heap até ele ser destruído.
Cada nova submissão marcada ainda usa uma rodada de submissão. Escritas
posteriores ao primeiro acesso continuam no journal. As falhas de leitura
deixam a página pendente para outra rodada.

Para um índice calculado em shader, a faixa conservadora é
`0..min(title_count, host_count)`. A análise de `heap_index` e
`fetched_index` em `victory-shader/src/emit/lower.rs` mostra a soma
de offsets no shader, que podem vir de memória. O clamp e os twins do
host limitam onde o descritor é representado, mas não identificam que
slot do título será escolhido. `SLOT_STRIDES`, heap emprestado, carga
dinâmica, proveniência desconhecida e código fora da análise também
mantêm a faixa cheia. `SLOT_TABLE_STRIDES` mantém o sinal de submissão.
O acesso à tabela de strides por si só só identifica a origem de tabela.

Assim que uma faixa alcança um heap misto, todas as páginas espelháveis
desse heap continuam elegíveis, inclusive o ring em `0x240f0000` se um
shader puder alcançá-lo. A porcentagem de registros de tabela é uma medida
de atividade. Excluir uma página exige prova de que o shader não pode
alcançá-la. O ganho nesse heap
vem da cadência, do corte do fim além do limite do host quando houver e
do comparador. As reduções maiores de páginas dentro do mesmo heap
dependem de intervalos estáticos mais estreitos ou feedback de GPU
com uma estratégia segura para a primeira leitura.

Na fase de comparação dos registros copiados, uma igualdade exata do intervalo resolve o caso
limpo. Caso haja diferenças, testam-se grupos de 512 bytes e só os
grupos diferentes entram na comparação de blocos de 64 bytes. Só os
blocos diferentes entram no laço por registro. O caminho reutiliza os
buffers de leitura e índices alterados por worker. A consulta de uma
forma inlined do mesmo descritor pode ser evitada quando a chave rápida
já coincide. O índice de formas é imutável após ser emitido. Entradas
recicláveis continuam a validar geração. Alterações reais usam a mesma
fila de texturas, seus atalhos e publicação do caminho legado.

R4 depende dos mesmos checks de geração, invalidação de memória, espera
por cópia, descritor neutro e publicação depois da prontidão/upload.
Nenhum slot é publicado diretamente pelo filtro ou pelo comparador.
Detalhes de `VICTORY_HEAP_SYNC=all`, heaps desalinhados e filas sem a
rota late impedem o filtro. As consultas de write-watch ainda passam pelo
journal legado por heap. Não há compartilhamento do estado de
`ask_ahead` com a thread de submissão.

### `VICTORY_RF_X4_VERIFY=1`

Cada rodada também executa a consulta integral legada com um cursor
independente e compara os registros com uma cópia de referência. O
verificador nunca escreve o heap da GPU. Ele conserva as diferenças que
encontrou em heaps omitidos para conferir na primeira ativação. Confere
os words do registro lido e, quando a política legada publicaria a view
imediatamente, a chave publicada. Tabelas recentes, buffers e uploads
pendentes preservam a política de espera. `rf-x4 diverge` inclui heap,
página, slot, words esperados e lidos, e chave publicada quando diverge.

Há custo adicional de leitura, memória e sincronização. Uma escrita
concorrente pode cair entre as duas consultas e gerar uma divergência
de temporização. Repetir a cena e conferir as palavras e a sequência
de rodadas ajuda a distinguir esse caso. O comparador valida a identidade
dos registros e chaves. A inspeção visual continua necessária para
validar conteúdo de textura e transições.

## Validação e A/B

O orquestrador executa os testes no Windows.

```powershell
cargo test -p victory-gpu --lib rf_x4::tests
```

Use o mesmo executável, save, cena pesada, modo Series S e configuração de
frame generation. Aqueça a cena e compare janelas com a mesma duração.
Grave os switches no `%LOCALAPPDATA%\victory\victory-settings.txt`, uma
linha `CHAVE=valor` por switch. O launcher lê esse arquivo e ignora
o ambiente. Reinicie o processo para cada variante.

| Variante | `VICTORY_RF_X4_STATS` | `VICTORY_RF_X4_PICK_GATE` | `VICTORY_RF_X4_PICKED` | `VICTORY_RF_X4_VERIFY` |
| --- | --- | --- | --- | --- |
| A | 1 | 0 | 0 | 0 |
| B | 1 | 1 | 1 | 1 |
| C | 1 | 1 | 1 | 0 |

- Em A, registre rodadas, páginas, bytes, tempos e proporções de
  `possible-picked` e `table-only`. Use `VICTORY_RF_X3_STATS=1` em A
  e C para comparar o mesmo heap em `descriptor-sync`.
- Em B, procure `rf-x4 diverge` e defeitos visuais, especialmente na
  entrada do mundo, mudanças de câmera, transições de mip, água e
  streaming. Observe os primeiros draws e dispatches em heaps recém-usados.
- Compare C com A no FPS, no `preparing` da linha de FPS, em
  `descriptor-sync` da linha `rf-x3:` e em rodadas, páginas, MB e tempos
  da linha `rf-x4:`. O diagnóstico de B tem custo próprio. Se a faixa
  potencial cobrir quase todas as páginas de ring, a expectativa de
  redução fica restrita ao gate, aos cortes de limite e à comparação.
