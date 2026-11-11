| part | metric | spared | celld-single | celld-fleet |
|---|---|---|---|---|
| cold | first byte p50 us | 3807 [3251-4102] (n=5, errors 0) | 1.437e+04 [1.388e+04-1.604e+04] (n=5, errors 0) | 1.348e+04 [1.159e+04-1.382e+04] (n=5, errors 0) |
| cold | first byte p99 us | 1.066e+04 [8809-1.212e+04] (n=5, errors 0) | 3.834e+04 [3.312e+04-4.176e+04] (n=5, errors 0) | 3.737e+04 [3.225e+04-4.068e+04] (n=5, errors 0) |
| cold-existing | first byte p50 us | 881 [786-936] (n=5, errors 0) | 1.462e+04 [1.41e+04-1.598e+04] (n=5, errors 0) | 1.493e+04 [1.371e+04-1.584e+04] (n=5, errors 0) |
| cold-existing | first byte p99 us | 2034 [1714-4374] (n=5, errors 0) | 3.786e+04 [3.043e+04-4.356e+04] (n=5, errors 0) | 3.932e+04 [3.332e+04-4.009e+04] (n=5, errors 0) |
| warm-c1 | first byte p50 us | 132 [127-159] (n=5, errors 0) | 404 [391-417] (n=5, errors 0) | 407 [384-427] (n=5, errors 0) |
| warm-c1 | first byte p99 us | 418 [383-443] (n=5, errors 0) | 1611 [1436-1673] (n=5, errors 0) | 1556 [1222-1774] (n=5, errors 0) |
| warm-c16 | first byte p50 us | 1821 [1820-2177] (n=5, errors 0) | 3935 [3816-4399] (n=5, errors 0) | 4439 [4124-4808] (n=5, errors 0) |
| warm-c16 | first byte p99 us | 5589 [5340-6003] (n=5, errors 0) | 9594 [9199-1.337e+04] (n=5, errors 0) | 1.081e+04 [1.025e+04-1.428e+04] (n=5, errors 0) |
| mixed-c1 | first byte p50 us | 154 [148-166] (n=5, errors 0) | 488 [466-505] (n=5, errors 0) | 476 [457-494] (n=5, errors 0) |
| mixed-c1 | first byte p99 us | 1888 [1734-1968] (n=5, errors 0) | 5468 [5033-5857] (n=5, errors 0) | 3216 [2937-3330] (n=5, errors 0) |
| mixed-c16 | first byte p50 us | 3903 [3488-4602] (n=5, errors 0) | 6820 [6319-9621] (n=5, errors 0) | 6060 [5836-6268] (n=5, errors 0) |
| mixed-c16 | first byte p99 us | 9237 [6864-9550] (n=5, errors 0) | 1.522e+04 [1.359e+04-2.205e+04] (n=5, errors 0) | 1.512e+04 [1.321e+04-1.998e+04] (n=5, errors 0) |

PSS MiB (sum over the process tree; celld-fleet = both nodes; bucket store not included):
| point | spared | celld-single | celld-fleet |
|---|---|---|---|---|
| empty | 13.08 [12.91-13.49] | 36.52 [36.33-36.7] | 51.24 [50.86-51.89] |
| n1 | 20.01 [19.89-20.35] | 50.85 [50.59-51.16] | 61.36 [60.9-64.69] |
| n10 | 22.47 [22.34-22.81] | 55.79 [55.63-55.83] | 70.11 [69.09-70.51] |
| n50 | 32.97 [32.88-33.36] | 68.49 [68.27-68.73] | 83.24 [82.93-83.9] |
per-app growth between N=1 and N=50 (MiB/app, (pss50-pss1)/49):
  spared 0.2651 [0.2647-0.2655]
  celld-single 0.3608 [0.3561-0.365]
  celld-fleet 0.4556 [0.3724-0.4575]
RSS MiB at n50: {'spared': '41.4 [41.31-41.82]', 'celld-single': '70.36 [70.16-70.61]', 'celld-fleet': '110.4 [109.9-110.9]'}
