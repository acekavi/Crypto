import unittest

from scripts.portfolio_status import build_portfolio_manifest


class PortfolioStatusTests(unittest.TestCase):
    def test_build_portfolio_manifest_summarizes_live_three_bot_portfolio(self):
        profiles = [
            {'name': 'AAVE/ETH', 'bot_id': 'aave_eth', 'service': 'crypto-bot-aave-eth.service', 'params': type('P', (), {'leg_a': 'AAVEUSDT', 'leg_b': 'ETHUSDT'})()},
            {'name': 'ENA/XRP', 'bot_id': 'ena_xrp', 'service': 'crypto-bot-ena-xrp.service', 'params': type('P', (), {'leg_a': 'ENAUSDT', 'leg_b': 'XRPUSDT'})()},
            {'name': 'BNB/XAUT', 'bot_id': 'bnb_xaut', 'service': 'crypto-bot-bnb-xaut.service', 'params': type('P', (), {'leg_a': 'BNBUSDT', 'leg_b': 'XAUTUSDT'})()},
        ]
        manifest = build_portfolio_manifest(profiles)
        self.assertEqual(manifest['pair_count'], 3)
        self.assertFalse(manifest['has_symbol_overlap'])
        self.assertEqual(
            manifest['symbols'],
            ['AAVEUSDT', 'BNBUSDT', 'ENAUSDT', 'ETHUSDT', 'XAUTUSDT', 'XRPUSDT'],
        )
        self.assertEqual(
            manifest['services'],
            [
                'crypto-bot-aave-eth.service',
                'crypto-bot-bnb-xaut.service',
                'crypto-bot-ena-xrp.service',
            ],
        )


if __name__ == '__main__':
    unittest.main()
