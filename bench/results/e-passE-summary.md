| metric | before median (min-max) | after median (min-max) | delta | paired rounds worse/better | verdict |
|---|---|---|---|---|---|
| warm-c1 p50 us | 162.5 (152.0-168.0) | 163.0 (153.0-166.0) | +0.3% | 2/4 of 6 | within noise |
| warm-c1 p99 us | 410.5 (339.0-453.0) | 363.0 (307.0-549.0) | -11.6% | 2/4 of 6 | within noise |
| warm-c1 req/s | 5579 (5316-5933) | 5564 (5403-5795) | -0.3% | 3/3 of 6 | within noise |
| warm-c16 p50 us | 2478 (1842-2526) | 2370 (1921-2495) | -4.4% | 2/4 of 6 | within noise |
| warm-c16 p99 us | 5584 (5147-5662) | 5636 (5196-6061) | +0.9% | 4/2 of 6 | within noise |
| warm-c16 req/s | 5371 (5202-6532) | 5615 (5223-6428) | +4.5% | 2/4 of 6 | within noise |
| mixed-c1 p50 us | 196.5 (176.0-200.0) | 188.0 (172.0-203.0) | -4.3% | 2/4 of 6 | within noise |
| mixed-c1 p99 us | 2572 (2265-2944) | 2506 (2258-3012) | -2.5% | 3/3 of 6 | within noise |
| mixed-c1 req/s | 2573 (2462-2743) | 2678 (2403-2759) | +4.1% | 2/4 of 6 | within noise |
| mixed-c16 p50 us | 4513 (4267-4934) | 4414 (4160-5173) | -2.2% | 3/3 of 6 | within noise |
| mixed-c16 p99 us | 11734 (8895-14714) | 11576 (10156-15922) | -1.3% | 2/4 of 6 | within noise |
| mixed-c16 req/s | 3050 (2702-3310) | 3129 (2550-3363) | +2.6% | 2/4 of 6 | within noise |
| noop p50 us | 70.5 (67.0-81.0) | 78.0 (70.0-81.0) | +10.6% | 4/2 of 6 | within noise |
| noop p99 us | 220.5 (143.0-339.0) | 341.0 (197.0-455.0) | +54.6% | 5/1 of 6 | within noise |
| noop server cpu us/req | 57.5 (52.0-64.0) | 60.0 (57.0-63.0) | +4.3% | 3/2 of 6 | within noise |
| sql-agg p50 us | 15792 (15199-16167) | 16221 (15175-16463) | +2.7% | 4/2 of 6 | within noise |
| sql-agg p99 us | 25574 (21731-32326) | 26776 (21030-34188) | +4.7% | 3/3 of 6 | within noise |
| sql-agg server cpu us/req | 15446 (14983-15665) | 15712 (14948-15858) | +1.7% | 4/2 of 6 | within noise |
| sql-write p50 us | 12580 (11188-13253) | 12391 (11331-12836) | -1.5% | 4/2 of 6 | within noise |
| sql-write p99 us | 23836 (19885-26264) | 22215 (21540-29542) | -6.8% | 3/3 of 6 | within noise |
| sql-write server cpu us/req | 9668 (9064-9919) | 9638 (9192-9944) | -0.3% | 3/3 of 6 | within noise |
| file-write p50 us | 3454 (3389-3515) | 3645 (3418-3749) | +5.5% | 4/2 of 6 | within noise |
| file-write p99 us | 9012 (7819-10249) | 8828 (7663-10224) | -2.0% | 3/3 of 6 | within noise |
| file-write server cpu us/req | 2988 (2952-3034) | 3155 (2997-3252) | +5.6% | 5/1 of 6 | within noise |
| file-small p50 us | 1756 (1557-1900) | 1698 (1573-1781) | -3.3% | 2/4 of 6 | within noise |
| file-small p99 us | 4922 (2554-5640) | 4680 (4010-5108) | -4.9% | 3/3 of 6 | within noise |
| file-small server cpu us/req | 1665 (1490-1790) | 1617 (1520-1692) | -2.9% | 3/3 of 6 | within noise |
| post-1kb p50 us | 101.0 (89.0-104.0) | 90.5 (86.0-104.0) | -10.4% | 0/5 of 6 | within noise |
| post-1kb p99 us | 240.5 (200.0-546.0) | 329.5 (250.0-570.0) | +37.0% | 4/2 of 6 | within noise |
| post-1kb server cpu us/req | 81.5 (80.0-88.0) | 78.5 (71.0-85.0) | -3.7% | 1/5 of 6 | within noise |
| post-1mb p50 us | 795.5 (701.0-819.0) | 679.0 (637.0-721.0) | -14.6% | 0/6 of 6 | within noise |
| post-1mb p99 us | 2029 (1119-2639) | 1457 (1333-2680) | -28.2% | 3/3 of 6 | within noise |
| post-1mb server cpu us/req | 619.5 (566.0-649.0) | 561.5 (531.0-587.0) | -9.4% | 0/6 of 6 | within noise |

Steal % per segment (median, max): after/a: 1.06, 1.16; after/b: 0.72, 0.85; before/a: 0.83, 1.22; before/b: 0.70, 0.86
